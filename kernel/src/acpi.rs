//! ACPI discovery: the CPUs, interrupt controllers and HPET the firmware
//! describes, read once at boot.
//!
//! Limine gives the RSDP's physical address; `lumen_acpi` walks RSDP → XSDT
//! (or RSDT) → MADT and HPET and checks every table it uses. Tables are
//! read through [`Firmware`], which only reads memory that holds firmware
//! data: memory the memory map gives to firmware (`acpi-*` and `reserved`
//! entries, outside the HHDM) is mapped uncached with `map_mmio` (a read
//! inside a span already mapped reuses that mapping); RAM the HHDM covers is
//! read there only if it is usable memory reserved for ACPI tables (it is
//! reserved on first read, under the name the RSDP's boot reservation uses,
//! so the allocator never hands it out: hard rule 2). What the kernel needs
//! is copied into [`Info`]; nothing keeps pointing into firmware memory.
//!
//! No ACPI, or broken tables, is not fatal: boot goes on without the
//! information, with a warning.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;

use limine::memory_map::EntryType;
use log::{debug, info, warn};
use lumen_acpi::{Acpi, Cpu, CpuState, Error, Hpet, InterruptOverride, IoApic, Madt, PhysMem, Signature};
use lumen_mm::{PhysAddr, PhysRange};

use crate::boot;
use crate::mm::{self, frame, paging};
use crate::sync::SpinLock;

/// Memory-map entry types firmware keeps its tables in. None is in the HHDM.
const FIRMWARE_TYPES: [EntryType; 3] = [EntryType::ACPI_RECLAIMABLE, EntryType::ACPI_NVS, EntryType::RESERVED];

/// What the kernel knows from ACPI.
#[derive(Debug)]
pub struct Info {
    /// Every CPU the MADT lists, whatever its state, in MADT order: one per
    /// CPU entry, so a CPU listed as both type 0 and type 9 appears twice
    /// (warned at boot).
    pub cpus: Vec<Cpu>,
    /// The local APIC's physical address (the MADT's type 5 address if it
    /// has one). 0 if the MADT says so (warned at boot): check before
    /// mapping it.
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
        self.cpus.iter().filter(|c| c.state == CpuState::Enabled)
    }
}

/// Whether [`init`] has run, and what it found.
#[derive(Clone, Copy)]
enum Discovery {
    NotYet,
    Absent,
    Found(&'static Info),
}

static DISCOVERY: SpinLock<Discovery> = SpinLock::new(Discovery::NotYet);

/// What ACPI described, or `None` if the firmware gave nothing usable.
/// Panics before [`init`] (a boot-order bug, not missing ACPI).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: the APIC timer (#7), SMP (#9)
pub fn info() -> Option<&'static Info> {
    match *DISCOVERY.lock() {
        Discovery::NotYet => panic!("acpi::info called before acpi::init"),
        Discovery::Absent => None,
        Discovery::Found(info) => Some(info),
    }
}

/// Why the memory behind a table couldn't be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    NotPhysical(u64),
    /// Neither firmware memory nor RAM the HHDM covers (or partly each).
    NotFirmware,
    Mmio(paging::MmioError),
    /// RAM the HHDM covers, which can't be reserved for ACPI tables: an
    /// allocated frame (the table may be overwritten), memory that holds
    /// something else, or a full reservation list.
    Reserve(frame::ReserveError),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::NotPhysical(a) => write!(f, "{a:#x} is not a physical address"),
            ReadError::NotFirmware => f.write_str("it is not wholly in firmware memory or wholly in usable memory"),
            ReadError::Mmio(e) => write!(f, "mapping it: {e}"),
            ReadError::Reserve(e) => write!(f, "reserving it: {e}"),
        }
    }
}

/// Physical memory as the ACPI parser reads it. Remembers which reads
/// failed and why (the parser only learns that one did).
#[derive(Debug, Default)]
pub struct Firmware {
    /// Each span `map_mmio` mapped, to read inside it again without
    /// remapping.
    mapped: Vec<Mapped>,
    /// `(address, why)` of each failed read not yet reported.
    failed: Vec<(u64, ReadError)>,
}

/// A span [`map_mmio`](paging::map_mmio) mapped: its whole pages, and the
/// virtual address of the first.
#[derive(Debug)]
struct Mapped {
    pages: PhysRange,
    virt: u64,
}

impl Firmware {
    /// Make `[addr, addr + len)` readable for the kernel's life, refusing
    /// memory that doesn't hold firmware data (see the module doc).
    fn map(&mut self, addr: u64, len: usize) -> Result<&'static [u8], ReadError> {
        let phys = PhysAddr::new(addr).ok_or(ReadError::NotPhysical(addr))?;
        let end = addr.checked_add(len as u64).ok_or(ReadError::NotPhysical(addr))?;
        let range = PhysRange::new(addr, end);
        let virt = if let Some(m) = self.mapped.iter().find(|m| m.pages.start() <= addr && end <= m.pages.end()) {
            m.virt + (addr - m.pages.start())
        } else if paging::hhdm_covers(phys, len as u64) {
            frame::reserve(frame::ACPI_TABLE, range).map_err(ReadError::Reserve)?;
            mm::phys_to_virt(phys).as_u64()
        } else if in_firmware_memory(addr, end) {
            let virt = paging::map_mmio(phys, len as u64).map_err(ReadError::Mmio)?.as_u64();
            let pages = range.grow_to_frames();
            self.mapped.push(Mapped { pages, virt: virt - (addr - pages.start()) });
            virt
        } else {
            return Err(ReadError::NotFirmware);
        };
        // SAFETY: `virt` maps `len` readable bytes for the kernel's whole
        // life: `map_mmio` mappings are permanent, and so is the HHDM. The
        // bytes are firmware data nothing in the kernel writes: an
        // `acpi-*`/`reserved` entry (the kernel never writes those), or
        // usable frames now reserved for ACPI tables (the allocator never
        // hands them out again). That they are tables and not, say, device
        // registers in a `reserved` entry, we trust the firmware's pointers.
        // The tables read here (the RSDP, the root table and the tables it
        // lists) are static; the FACS, which firmware writes, is never read.
        Ok(unsafe { core::slice::from_raw_parts(virt as *const u8, len) })
    }

    /// Why the last read at `addr` failed, if one did; that failure (and
    /// any earlier one at `addr`) counts as reported.
    fn take_why(&mut self, addr: u64) -> Option<ReadError> {
        let why = self.failed.iter().rev().find(|f| f.0 == addr).map(|f| f.1);
        self.failed.retain(|f| f.0 != addr);
        why
    }

    /// Warn about each failed read not reported yet: tables skipped while
    /// looking for others.
    fn warn_unreported(&mut self) {
        while let Some(&(addr, _)) = self.failed.first() {
            let why = self.take_why(addr).expect("just seen");
            warn!(target: "acpi", "skipped the table at {addr:#x}: {why}");
        }
    }
}

impl PhysMem<'static> for Firmware {
    fn read(&mut self, addr: u64, len: usize) -> Option<&'static [u8]> {
        self.map(addr, len).inspect_err(|&e| self.failed.push((addr, e))).ok()
    }
}

/// Is `[addr, end)` covered by memory-map entries of [`FIRMWARE_TYPES`]?
fn in_firmware_memory(addr: u64, end: u64) -> bool {
    let Some(entries) = boot::MEMMAP.get_response().map(|r| r.entries()) else {
        return false;
    };
    let mut at = addr;
    while at < end {
        let entry = entries
            .iter()
            .find(|e| FIRMWARE_TYPES.contains(&e.entry_type) && e.base <= at && at < e.base.saturating_add(e.length));
        match entry {
            Some(e) => at = e.base.saturating_add(e.length),
            None => return false,
        }
    }
    true
}

/// An ACPI failure, with the reason the read it names failed, if one did.
struct Failure {
    error: Error,
    cause: Option<ReadError>,
}

impl Failure {
    /// `error`, with why the read it names failed (which then counts as
    /// reported).
    fn new(error: Error, fw: &mut Firmware) -> Failure {
        let cause = match error {
            Error::Unreadable { addr, .. } | Error::NotFound { skipped: Some(addr), .. } => fw.take_why(addr),
            _ => None,
        };
        Failure { error, cause }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)?;
        if let Some(cause) = self.cause {
            write!(f, " ({cause})")?;
        }
        Ok(())
    }
}

/// Read the MADT and HPET, log a summary, and keep the result for [`info`].
/// Needs the heap and `map_mmio` (after `mm::init`). Panics if called
/// twice.
pub fn init() {
    {
        let mut d = DISCOVERY.lock();
        assert!(matches!(*d, Discovery::NotYet), "acpi::init called twice");
        *d = Discovery::Absent;
    }
    let Some(rsdp) = boot::RSDP.get_response() else {
        warn!(target: "acpi", "running without ACPI: no RSDP from Limine");
        return;
    };
    let mut fw = Firmware::default();
    let found = discover(&mut fw, rsdp.address() as u64);
    if let Err(error) = found {
        warn!(target: "acpi", "running without ACPI: {}", Failure::new(error, &mut fw));
    }
    fw.warn_unreported();
    let Ok(info) = found else {
        return;
    };
    let cpus = info.enabled_cpus().count();
    let plural = if cpus == 1 { "" } else { "s" };
    let lapic = info.local_apic;
    match info.hpet {
        Some(hpet) => info!(target: "acpi", "{cpus} CPU{plural}, LAPIC at {lapic:#x}, HPET at {:#x}", hpet.address),
        None => info!(target: "acpi", "{cpus} CPU{plural}, LAPIC at {lapic:#x}, no HPET"),
    }
    *DISCOVERY.lock() = Discovery::Found(Box::leak(Box::new(info)));
}

/// Walk the tables from the RSDP at `rsdp`. A missing or broken HPET is
/// logged and left out; a bad RSDP or root table, or a missing or broken
/// MADT, fails.
fn discover(fw: &mut Firmware, rsdp: u64) -> Result<Info, Error> {
    let mut acpi = Acpi::new(&mut *fw, rsdp)?;
    if log::log_enabled!(target: "acpi", log::Level::Debug) {
        log_tables(&mut acpi);
    }
    let madt = Madt::parse(acpi.find(Signature::MADT)?)?;
    let hpet = acpi.find(Signature::HPET).and_then(Hpet::parse);
    let fw = acpi.into_mem();
    let hpet = hpet.inspect_err(|&e| warn!(target: "acpi", "no usable HPET: {}", Failure::new(e, &mut *fw))).ok();
    let info = Info {
        cpus: madt.cpus().collect(),
        local_apic: madt.local_apic_address(),
        io_apics: madt.io_apics().collect(),
        overrides: madt.interrupt_overrides().collect(),
        has_8259_pics: madt.has_8259_pics(),
        hpet,
    };
    for c in &info.cpus {
        debug!(target: "acpi", "CPU uid {} APIC id {} {}", c.uid, c.apic_id, c.state);
    }
    let boot_cpu = crate::arch::apic_id();
    if !info.enabled_cpus().any(|c| c.apic_id == boot_cpu) {
        warn!(target: "acpi", "the boot CPU (APIC id {boot_cpu}) is not an enabled CPU in the MADT");
    }
    if info.local_apic == 0 {
        warn!(target: "acpi", "the MADT gives the local APIC's address as 0");
    }
    let mut ids: Vec<u32> = info.enabled_cpus().map(|c| c.apic_id).collect();
    ids.sort_unstable();
    if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
        warn!(target: "acpi", "the MADT lists APIC id {} as more than one enabled CPU", w[0]);
    }
    if info.io_apics.is_empty() {
        warn!(target: "acpi", "the MADT lists no I/O APIC");
    }
    for io in &info.io_apics {
        info!(target: "acpi", "I/O APIC {} at {:#x}, GSI base {}", io.id, io.address, io.gsi_base);
    }
    debug!(target: "acpi", "{} interrupt overrides, 8259 PICs {}", info.overrides.len(), info.has_8259_pics);
    Ok(info)
}

/// Log the root table's signature and what it lists (debug level only: it
/// reads, and so maps or reserves, every listed table's header).
fn log_tables(acpi: &mut Acpi<'static, &mut Firmware>) {
    use core::fmt::Write;
    let (root, revision) = (acpi.root().signature(), acpi.rsdp().revision);
    let mut line = alloc::string::String::new();
    for sig in acpi.signatures() {
        match sig {
            Ok(s) => write!(line, " {s}"),
            Err(e) => write!(line, " ({e})"),
        }
        .expect("writing to a String can't fail");
    }
    debug!(target: "acpi", "revision {revision}, {root} lists{line}");
}

crate::ktest!(
    fn madt_lists_boot_cpu() {
        use crate::kassert_eq;
        let info = info().ok_or("ACPI was not found at boot")?;
        let id = crate::arch::apic_id();
        let boot_cpu = info.cpus.iter().find(|c| c.apic_id == id);
        if boot_cpu.is_none() {
            crate::ktest::diag(format_args!("boot CPU APIC id {id}, MADT lists {:?}", info.cpus));
        }
        kassert_eq!(boot_cpu.map(|c| c.state), Some(CpuState::Enabled));
        Ok(())
    }
);

crate::ktest!(
    fn info_copies_what_q35_describes() {
        // QEMU q35's MADT and HPET (hw/i386/acpi-common.c): one I/O APIC at
        // 0xfec00000 handling GSIs from 0, ISA IRQ 0 (the PIT) wired to GSI 2,
        // the 8259 PICs present, the HPET at 0xfed00000.
        use crate::{kassert, kassert_eq};
        let info = info().ok_or("ACPI was not found at boot")?;
        kassert_eq!(info.local_apic, 0xfee0_0000);
        kassert_eq!(info.io_apics.as_slice(), &[IoApic { id: 0, address: 0xfec0_0000, gsi_base: 0 }]);
        kassert!(info.overrides.iter().any(|o| (o.bus, o.irq, o.gsi) == (0, 0, 2)));
        kassert!(info.has_8259_pics);
        kassert_eq!(info.hpet.map(|h| h.address), Some(0xfed0_0000));
        Ok(())
    }
);

crate::ktest!(
    fn tables_outside_the_hhdm_are_mapped_uncached_once() {
        use lumen_mm::paging::Cache;

        use crate::kassert_eq;
        let rsdp = boot::RSDP.get_response().ok_or("no RSDP")?.address() as u64;
        let phys = PhysAddr::new(rsdp).ok_or("RSDP not physical")?;
        let mut fw = Firmware::default();
        let bytes = fw.map(rsdp, 20).map_err(|_| "mapping the RSDP failed")?;
        kassert_eq!(&bytes[..8], b"RSD PTR ");
        let virt = lumen_mm::VirtAddr::new(bytes.as_ptr() as u64).ok_or("not canonical")?;
        let m = paging::walk(virt).mapping().ok_or("not mapped")?;
        kassert_eq!(m.phys_of(virt), Some(phys));
        kassert_eq!(m.cache(), Some(Cache::Uncached));
        // Reading more of the same page uses the same mapping.
        let again = fw.map(rsdp + 8, 12).map_err(|_| "second read failed")?;
        kassert_eq!(again.as_ptr() as u64, bytes.as_ptr() as u64 + 8);
        kassert_eq!(fw.mapped.len(), 1);
        Ok(())
    }
);

crate::ktest!(
    fn tables_in_usable_memory_are_reserved_or_refused() {
        // Stand-ins for a table in usable memory: a free frame (reserved,
        // then read through the HHDM) and an allocated one (refused).
        use crate::{kassert, kassert_eq};
        let mut fw = Firmware::default();
        let held = frame::alloc_owned().ok_or("out of frames")?;
        let f = held.frame();
        let addr = f.as_u64();
        kassert_eq!(fw.map(addr, 36), Err(ReadError::Reserve(frame::ReserveError::InUse(addr))));
        kassert!(frame::is_allocated(f)); // a refused read changes nothing
        drop(held);
        // The frame is free again: now it is reserved for ACPI tables.
        let free_before = frame::free_frames();
        let bytes = fw.map(addr, 36).map_err(|_| "reading a free frame failed")?;
        kassert_eq!(bytes.as_ptr() as u64, mm::phys_to_virt(f.start()).as_u64());
        kassert_eq!(frame::free_frames(), free_before - 1);
        kassert!(!frame::is_allocated(f));
        // Reading it again takes nothing more.
        fw.map(addr, 36).map_err(|_| "second read failed")?;
        kassert_eq!(frame::free_frames(), free_before - 1);
        Ok(())
    }
);

crate::ktest!(
    fn memory_that_holds_something_else_is_refused() {
        // A bad table pointer must not hand out a view of memory the kernel
        // writes, nor of nothing at all.
        use crate::kassert_eq;
        let mut fw = Firmware::default();
        let bitmap = frame::bitmap().ok_or("no frame bitmap")?.start();
        kassert_eq!(
            fw.map(bitmap, 36),
            Err(ReadError::Reserve(frame::ReserveError::ReservedFor { frame: bitmap, name: frame::BITMAP }))
        );
        let (fb, _) = frame::framebuffer_virt().ok_or("no framebuffer")?;
        let fb = paging::translate(lumen_mm::VirtAddr::new(fb).ok_or("fb")?).ok_or("fb unmapped")?.as_u64();
        kassert_eq!(
            fw.map(fb, 36),
            Err(ReadError::Reserve(frame::ReserveError::ReservedFor { frame: fb, name: frame::FRAMEBUFFER }))
        );
        // Far above RAM: in no memory-map entry.
        kassert_eq!(fw.map(1 << 40, 36), Err(ReadError::NotFirmware));
        kassert_eq!(fw.map(u64::MAX - 8, 36), Err(ReadError::NotPhysical(u64::MAX - 8)));
        // A read that runs off the end of firmware memory into something
        // else (or a gap), and one from usable memory into firmware memory.
        let entries = boot::MEMMAP.get_response().ok_or("no memory map")?.entries();
        let is_fw = |e: &limine::memory_map::Entry| FIRMWARE_TYPES.contains(&e.entry_type);
        let last_fw =
            entries.windows(2).find(|w| is_fw(w[0]) && !(is_fw(w[1]) && w[1].base == w[0].base + w[0].length));
        let end = last_fw.map(|w| w[0].base + w[0].length).ok_or("no firmware entry followed by other memory")?;
        kassert_eq!(fw.map(end - 16, 32), Err(ReadError::NotFirmware));
        let into_fw = entries
            .windows(2)
            .find(|w| w[0].entry_type == EntryType::USABLE && is_fw(w[1]) && w[0].base + w[0].length == w[1].base);
        let start = into_fw.map(|w| w[1].base).ok_or("no usable entry right below a firmware one")?;
        kassert_eq!(fw.map(start - 16, 32), Err(ReadError::NotFirmware));
        Ok(())
    }
);

crate::ktest!(
    fn a_failed_walk_says_why() {
        use alloc::format;
        use alloc::string::ToString;

        use crate::kassert_eq;
        const WHY: &str = "it is not wholly in firmware memory or wholly in usable memory";
        let mut fw = Firmware::default();
        let error = discover(&mut fw, 1 << 40).err().ok_or("discovery found tables at 1 TiB")?;
        kassert_eq!(error, Error::Unreadable { addr: 1 << 40, len: 20 });
        kassert_eq!(
            Failure::new(error, &mut fw).to_string(),
            format!("can't read 20 bytes at {:#x} ({WHY})", 1u64 << 40)
        );
        kassert_eq!(fw.failed.len(), 0); // reported, so not warned again
        // A table skipped while looking for another.
        kassert_eq!(fw.read(1 << 40, 36), None);
        let skipped = Error::NotFound { sig: Signature::MADT, skipped: Some(1 << 40) };
        kassert_eq!(
            Failure::new(skipped, &mut fw).to_string(),
            format!("no APIC table (skipped an unreadable table at {:#x}) ({WHY})", 1u64 << 40)
        );
        // Errors that name no read get no cause.
        kassert_eq!(fw.read(1 << 40, 36), None);
        kassert_eq!(
            Failure::new(Error::NoRootTable, &mut fw).to_string(),
            "RSDP has neither an XSDT nor an RSDT address"
        );
        kassert_eq!(fw.failed.len(), 1); // still to be warned about
        Ok(())
    }
);

crate::ktest!(
    fn discovery_keeps_nothing_it_does_not_return() {
        // Walking the tables again (as `init` did) leaves the heap as it was
        // once the result is dropped: nothing leaks.
        use crate::{kassert, kassert_eq};
        let rsdp = boot::RSDP.get_response().ok_or("no RSDP")?.address() as u64;
        let before = mm::heap::stats();
        let mut fw = Firmware::default();
        let info = discover(&mut fw, rsdp).map_err(|_| "discovery failed")?;
        kassert!(info.enabled_cpus().count() > 0);
        drop((info, fw));
        let after = mm::heap::stats();
        kassert_eq!((after.used, after.live), (before.used, before.live));
        Ok(())
    }
);

crate::ktest!(
    fn hhdm_covers_checks_every_page() {
        use crate::kassert;
        let held = frame::alloc_owned().ok_or("out of frames")?;
        let p = held.frame().start();
        kassert!(paging::hhdm_covers(p, 4096));
        kassert!(!paging::hhdm_covers(p, 0));
        kassert!(!paging::hhdm_covers(p, u64::MAX));
        // A range that starts in usable memory and runs into firmware
        // memory (not in the HHDM), found from the memory map.
        let entries = boot::MEMMAP.get_response().ok_or("no memory map")?.entries();
        let next_to_firmware = entries.windows(2).find(|w| {
            w[0].entry_type == EntryType::USABLE
                && FIRMWARE_TYPES.contains(&w[1].entry_type)
                && w[0].base + w[0].length == w[1].base
        });
        let w = next_to_firmware.ok_or("no usable entry right below a firmware one")?;
        let last = PhysAddr::new(w[1].base - 4096).ok_or("phys")?;
        kassert!(paging::hhdm_covers(last, 4096));
        kassert!(!paging::hhdm_covers(last, 8192));
        Ok(())
    }
);

crate::ktest!(
    fn only_enabled_cpus_are_ready() {
        use crate::kassert_eq;
        let cpu = |apic_id, state| Cpu { apic_id, uid: apic_id, state };
        let info = Info {
            cpus: alloc::vec![
                cpu(0, CpuState::Enabled),
                cpu(1, CpuState::OnlineCapable),
                cpu(2, CpuState::Disabled),
                cpu(3, CpuState::Enabled),
            ],
            local_apic: 0xfee0_0000,
            io_apics: Vec::new(),
            overrides: Vec::new(),
            has_8259_pics: false,
            hpet: None,
        };
        let ids: Vec<u32> = info.enabled_cpus().map(|c| c.apic_id).collect();
        kassert_eq!(ids.as_slice(), &[0, 3]);
        Ok(())
    }
);
