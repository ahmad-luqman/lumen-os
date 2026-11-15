//! PCI discovery: every function in the MCFG's configuration space, read
//! once at boot into [`devices`] (docs/ARCHITECTURE.md, section 7).
//!
//! Configuration space is reached through ECAM only (the legacy `0xcf8`
//! ports aren't used): each accepted MCFG region's buses are mapped once,
//! uncached, with `map_mmio` (1 MiB per bus: 256 MiB for q35's 256 buses,
//! which the pinned OVMF moves from QEMU's 0xb0000000 to 0xe0000000),
//! behind an accessor private to this module. `lumen_pci` decodes headers,
//! sizes BARs and walks capability lists; the scan is brute force (every
//! bus in the range, devices 0-31, functions 1-7 only for multi-function
//! devices), so bridges need no recursion. No ACPI, no MCFG, an MCFG
//! without a usable region, or no region mapped means no PCI devices, with
//! a warning and the reason in [`status`] (which `lspci` prints), not a
//! crash. Each refused MCFG entry, unmapped region, and each function's
//! problems (`DeviceInfo::problems`: a capability list cut off or not
//! read, a BAR unassigned or malformed, a bridge whose bus numbers are
//! wrong or reach outside the mapped regions) is a warning.
//!
//! Enumeration also records every memory BAR in the BAR registry. The
//! registry maps a BAR (`map_bar`) only when asked, once, when a driver
//! claims the device (#95), and never unmaps it (`map_mmio`'s window only
//! grows). Nothing else maps BARs, except that the framebuffer Limine set
//! up inside the VGA BAR is in the HHDM (so `map_mmio` refuses that BAR).
//!
//! Locks: `REGISTRY` is taken before the page tables and the frame
//! allocator (`map_mmio`), and before the heap (the ktest clones it);
//! `ECAM` before `DISCOVERY` (the ECAM ktest); `DISCOVERY` is held only to
//! copy it out.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;

use log::{debug, info, warn};
use lumen_acpi::EcamRegion;
use lumen_mm::{PhysAddr, VirtAddr};
pub use lumen_pci::{Bar, BarProblem, DeviceInfo, NoBar, PciAddr};
use lumen_pci::{ConfigSpace, ecam_offset, enumerate};

use crate::acpi::{self, Failure};
use crate::mm::paging::{self, MmioError};
use crate::sync::SpinLock;

/// One accepted MCFG region, mapped at `virt` (its first bus).
struct Region {
    mcfg: EcamRegion,
    virt: VirtAddr,
}

/// Configuration space through ECAM: the mapped regions (no two share a
/// bus: the MCFG's overlapping entries were refused). Reads and writes of
/// a function outside them are a bug (every address comes from the scan of
/// these regions) and panic.
struct Ecam {
    regions: Vec<Region>,
}

impl Ecam {
    /// The dword at `offset` in `addr`'s configuration space. Panics for
    /// an offset that isn't a dword below 4096, or a function outside
    /// every region (both bugs).
    fn dword(&self, addr: PciAddr, offset: u16) -> *mut u32 {
        assert!(
            offset.is_multiple_of(4) && offset < 4096,
            "PCI config offset {offset:#x} for {addr} isn't a dword below 4096"
        );
        let at = self
            .regions
            .iter()
            .find_map(|r| ecam_offset(r.mcfg.segment(), r.mcfg.buses(), addr, offset).map(|o| r.virt.as_u64() + o));
        at.unwrap_or_else(|| panic!("PCI config of {addr} is outside every ECAM region")) as *mut u32
    }

    /// Whether `bus` of `segment` is in a mapped region.
    fn covers(&self, segment: u16, bus: u8) -> bool {
        self.regions.iter().any(|r| r.mcfg.covers(segment, bus))
    }
}

impl ConfigSpace for Ecam {
    fn read(&mut self, addr: PciAddr, offset: u16) -> u32 {
        // SAFETY: `dword` is inside a region `map_mmio` mapped uncached for
        // the kernel's life (4-byte aligned, within its function's 4 KiB).
        // Configuration space is device registers: a volatile access,
        // never cached or merged.
        unsafe { self.dword(addr, offset).read_volatile() }
    }

    fn write(&mut self, addr: PciAddr, offset: u16, value: u32) {
        // SAFETY: as in `read`. Only this module writes configuration
        // space (BAR sizing and command registers at enumeration).
        unsafe { self.dword(addr, offset).write_volatile(value) }
    }
}

/// Why there are no PCI devices.
#[derive(Clone, Copy, Debug)]
pub enum NoPci {
    /// `acpi::init` found nothing usable (no RSDP, a broken root table, no
    /// MADT, ...): there is no MCFG to read.
    NoAcpi,
    NoMcfg(Failure),
    /// The MCFG lists no entry, or refused all of them.
    NoRegions,
    /// No accepted region could be mapped.
    NoneMapped,
}

impl fmt::Display for NoPci {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoPci::NoAcpi => f.write_str("running without ACPI, so no MCFG"),
            NoPci::NoMcfg(e) => write!(f, "{e}"),
            NoPci::NoRegions => f.write_str("the MCFG lists no usable ECAM region"),
            NoPci::NoneMapped => f.write_str("no ECAM region could be mapped"),
        }
    }
}

/// What enumeration could reach.
#[derive(Clone, Copy, Debug)]
pub struct Status {
    /// Why there are no devices, if there are none for a reason.
    pub missing: Option<NoPci>,
    /// MCFG entries refused plus accepted regions that couldn't be mapped
    /// (each warned about at boot): devices there aren't listed.
    pub skipped: usize,
}

impl Status {
    /// Nothing missing, nothing skipped.
    pub fn is_clean(&self) -> bool {
        self.missing.is_none() && self.skipped == 0
    }

    /// What is missing, for a note: `no PCI: <reason>`, `2 ECAM regions
    /// skipped (refused or not mapped): see the boot log`, or both,
    /// separated by `; `. Empty when [`is_clean`](Self::is_clean).
    pub fn note(&self) -> impl fmt::Display + use<> {
        let Status { missing, skipped } = *self;
        fmt::from_fn(move |f| {
            if let Some(why) = missing {
                write!(f, "no PCI: {why}")?;
            }
            if skipped > 0 {
                let (sep, s) = (if missing.is_some() { "; " } else { "" }, if skipped == 1 { "" } else { "s" });
                write!(f, "{sep}{skipped} ECAM region{s} skipped (refused or not mapped): see the boot log")?;
            }
            Ok(())
        })
    }
}

/// ` (<note>)`, or nothing when clean: what `lspci`'s header adds.
impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_clean() { Ok(()) } else { write!(f, " ({})", self.note()) }
    }
}

/// Whether [`init`] has run, and what it found.
#[derive(Clone, Copy)]
enum Discovery {
    NotYet,
    Done { devices: &'static [DeviceInfo], status: Status },
}

static DISCOVERY: SpinLock<Discovery> = SpinLock::new(Discovery::NotYet);

/// The mapped configuration space, for #95's claim and its function
/// config (nothing outside this module reaches it).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by ktests until #95
static ECAM: SpinLock<Option<Ecam>> = SpinLock::new(None);

fn discovery() -> (&'static [DeviceInfo], Status) {
    match *DISCOVERY.lock() {
        Discovery::NotYet => panic!("pci used before pci::init"),
        Discovery::Done { devices, status } => (devices, status),
    }
}

/// Every PCI function found at boot, in address order; empty without
/// ECAM. Never changes after [`init`]. Panics before it (a boot-order bug).
pub fn devices() -> &'static [DeviceInfo] {
    discovery().0
}

/// Why [`devices`] is empty, or what it may be missing. Panics before
/// [`init`].
pub fn status() -> Status {
    discovery().1
}

/// Map each accepted MCFG region, scan it, record the memory BARs and log
/// what was found. After `acpi::init` (and `mm::init`), before any driver.
///
/// BAR sizing turns each device's decoding off for a moment, so this must
/// run while nothing can touch a device: on the boot CPU alone, with
/// interrupts off, before the framebuffer's terminal draws (the screen is
/// inside the VGA BAR). Nothing logs while a device's decoding is off
/// (`size_bars` doesn't). A panic now reports on serial only (there is no
/// crash screen before `fb::init`). Panics if called twice.
pub fn init() {
    assert!(matches!(*DISCOVERY.lock(), Discovery::NotYet), "pci::init called twice");
    let (devices, status) = discover();
    *DISCOVERY.lock() = Discovery::Done { devices: Box::leak(devices.into_boxed_slice()), status };
}

/// No devices, for `why` (warned).
fn none(why: NoPci, skipped: usize) -> (Vec<DeviceInfo>, Status) {
    warn!(target: "pci", "no PCI devices: {why}");
    (Vec::new(), Status { missing: Some(why), skipped })
}

fn discover() -> (Vec<DeviceInfo>, Status) {
    let table = match acpi::info().map(|info| &info.ecam) {
        None => return none(NoPci::NoAcpi, 0),
        Some(Err(e)) => return none(NoPci::NoMcfg(*e), 0),
        Some(Ok(table)) => table,
    };
    for bad in &table.refused {
        warn!(target: "pci", "{bad}");
    }
    if table.regions.is_empty() {
        return none(NoPci::NoRegions, table.refused.len());
    }
    let mut ecam = Ecam { regions: table.regions.iter().filter_map(map_region).collect() };
    let skipped = table.refused.len() + table.regions.len() - ecam.regions.len();
    if ecam.regions.is_empty() {
        return none(NoPci::NoneMapped, skipped);
    }
    let mut found = Vec::new();
    for i in 0..ecam.regions.len() {
        let m = ecam.regions[i].mcfg;
        enumerate(&mut ecam, m.segment(), m.buses(), |d| found.push(d));
    }
    found.sort_unstable_by_key(|d| d.addr);
    let mut bars = Vec::new();
    for d in &found {
        debug!(target: "pci", "{d}");
        for problem in d.problems(|segment, bus| ecam.covers(segment, bus)) {
            warn!(target: "pci", "{}: {problem}", d.addr);
        }
        for (index, bar) in d.bars.iter().enumerate() {
            if let Some(Bar::Memory { phys, size, .. }) = *bar {
                bars.push(BarEntry { addr: d.addr, index, phys, size, mapped: None });
            }
        }
    }
    let plural = if found.len() == 1 { "" } else { "s" };
    info!(
        target: "pci",
        "{} function{plural}, {} memory BARs, through ECAM: {}",
        found.len(),
        bars.len(),
        Regions(&ecam)
    );
    *REGISTRY.lock() = bars;
    *ECAM.lock() = Some(ecam);
    (found, Status { missing: None, skipped })
}

/// Map `r`'s buses, or warn and skip it.
fn map_region(r: &EcamRegion) -> Option<Region> {
    let mapped = PhysAddr::new(r.start()).ok_or(MmioError::BadRange).and_then(|p| paging::map_mmio(p, r.size()));
    match mapped {
        Ok(virt) => Some(Region { mcfg: *r, virt }),
        Err(e) => {
            warn!(
                target: "pci",
                "segment {} buses {}-{} skipped: mapping its ECAM at {:#x}: {e}",
                r.segment(),
                r.bus_start(),
                r.bus_end(),
                r.start()
            );
            None
        }
    }
}

/// `segment 0 buses 0-255 at 0xe0000000`, for each mapped region.
struct Regions<'a>(&'a Ecam);

impl fmt::Display for Regions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, r) in self.0.regions.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            write!(f, "{sep}{}", r.mcfg)?;
        }
        Ok(())
    }
}

/// A memory BAR enumeration found, and its mapping once asked for. (`phys`
/// stays a plain number: a BAR may claim an address past the physical
/// address space, which mapping it then refuses.)
#[derive(Clone, Copy, Debug)]
struct BarEntry {
    addr: PciAddr,
    index: usize,
    phys: u64,
    size: u64,
    mapped: Option<BarMapping>,
}

/// The BAR registry: every memory BAR, in address order. The one record
/// of BAR mappings.
static REGISTRY: SpinLock<Vec<BarEntry>> = SpinLock::new(Vec::new());

/// A memory BAR's mapping: uncached, for the kernel's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: #95's claim
pub struct BarMapping {
    phys: PhysAddr,
    virt: VirtAddr,
    size: u64,
}

#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: #95's claim
impl BarMapping {
    pub fn phys(&self) -> PhysAddr {
        self.phys
    }

    pub fn virt(&self) -> VirtAddr {
        self.virt
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

/// Why BAR `index` of `addr` has no mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: #95's claim
pub struct BarError {
    pub addr: PciAddr,
    pub index: usize,
    pub why: BarWhy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: #95's claim
pub enum BarWhy {
    /// Enumeration found no such function.
    NoSuchFunction,
    /// There are only BARs 0-5.
    OutOfRange,
    /// Not implemented.
    NotImplemented,
    /// The upper half of the 64-bit BAR before it.
    UpperHalf,
    /// An I/O BAR (nothing to map).
    Io,
    /// The firmware assigned it no address.
    Unassigned,
    Malformed(BarProblem),
    /// Not mapped yet (the lookup that doesn't map).
    NotMapped,
    /// Mapping `[phys, phys + size)` failed.
    Mmio {
        phys: u64,
        size: u64,
        error: MmioError,
    },
}

impl fmt::Display for BarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} BAR {}: ", self.addr, self.index)?;
        match self.why {
            BarWhy::NoSuchFunction => f.write_str("no such function"),
            BarWhy::OutOfRange => f.write_str("there are only BARs 0-5"),
            BarWhy::NotImplemented => f.write_str("not implemented"),
            BarWhy::UpperHalf => f.write_str("the upper half of a 64-bit BAR"),
            BarWhy::Io => f.write_str("an I/O BAR"),
            BarWhy::Unassigned => f.write_str("the firmware assigned it no address"),
            BarWhy::Malformed(why) => write!(f, "unusable: {why}"),
            BarWhy::NotMapped => f.write_str("not mapped yet"),
            BarWhy::Mmio { phys, size, error } => write!(f, "mapping {phys:#x} (+{size:#x}): {error}"),
        }
    }
}

/// Whether BAR `index` of `addr` is a memory BAR the registry holds; if
/// not, why. Takes no lock but `DISCOVERY`'s, briefly.
fn check_bar(addr: PciAddr, index: usize) -> Result<(), BarError> {
    let fail = |why| Err(BarError { addr, index, why });
    let Some(d) = devices().iter().find(|d| d.addr == addr) else { return fail(BarWhy::NoSuchFunction) };
    match d.bar_slot(index) {
        Ok(Bar::Memory { .. }) => Ok(()),
        Ok(Bar::Io { .. }) => fail(BarWhy::Io),
        Ok(Bar::Unassigned { .. }) => fail(BarWhy::Unassigned),
        Ok(Bar::Malformed(why)) => fail(BarWhy::Malformed(why)),
        Err(NoBar::OutOfRange) => fail(BarWhy::OutOfRange),
        Err(NoBar::UpperHalf) => fail(BarWhy::UpperHalf),
        Err(NoBar::NotImplemented) => fail(BarWhy::NotImplemented),
    }
}

/// Memory BAR `index` of `addr`, mapped (uncached) the first time it is
/// asked for; later calls return the same mapping. The registry's lock is
/// held across `map_mmio`, so two callers can't map one BAR twice.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first caller: #95's claim
fn map_bar(addr: PciAddr, index: usize) -> Result<BarMapping, BarError> {
    check_bar(addr, index)?;
    let mut registry = REGISTRY.lock();
    let e = registry.iter_mut().find(|e| e.addr == addr && e.index == index).expect("every memory BAR is registered");
    if let Some(m) = e.mapped {
        return Ok(m);
    }
    let (raw, size) = (e.phys, e.size);
    let fail = |error| BarError { addr, index, why: BarWhy::Mmio { phys: raw, size, error } };
    let phys = PhysAddr::new(raw).ok_or(fail(MmioError::BadRange))?;
    let virt = paging::map_mmio(phys, size).map_err(fail)?;
    Ok(*e.mapped.insert(BarMapping { phys, virt, size }))
}

/// Memory BAR `index` of `addr`'s mapping, if [`map_bar`] made one; maps
/// nothing (for #95's pre-probe check and `mmio_phys_to_virt`, and #96's
/// MSI-X table).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first caller: #95's probe
fn mapped_bar(addr: PciAddr, index: usize) -> Result<BarMapping, BarError> {
    check_bar(addr, index)?;
    let registry = REGISTRY.lock();
    let e = registry.iter().find(|e| e.addr == addr && e.index == index).expect("every memory BAR is registered");
    e.mapped.ok_or(BarError { addr, index, why: BarWhy::NotMapped })
}

crate::ktest!(
    fn q35s_chipset_is_found() {
        // QEMU q35: the host bridge at 00:00.0, and at 00:1f the ICH9's LPC
        // bridge (function 0, multi-function), SATA (2) and SMBus (3).
        use crate::kassert;
        let has = |d: u8, f: u8, vendor: u16, device: u16| {
            devices()
                .iter()
                .any(|i| i.addr == PciAddr::new(0, 0, d, f).expect("valid") && (i.vendor, i.device) == (vendor, device))
        };
        kassert!(has(0, 0, 0x8086, 0x29c0));
        kassert!(has(0x1f, 0, 0x8086, 0x2918));
        kassert!(has(0x1f, 2, 0x8086, 0x2922));
        kassert!(has(0x1f, 3, 0x8086, 0x2930));
        kassert!(devices().windows(2).all(|w| w[0].addr < w[1].addr));
        let guard = ECAM.lock();
        let ecam = guard.as_ref().ok_or("no ECAM")?;
        kassert!(devices().iter().all(|d| d.probeable() && d.problems(|s, b| ecam.covers(s, b)).count() == 0));
        let status = status();
        kassert!(status.missing.is_none() && status.skipped == 0);
        Ok(())
    }
);

crate::ktest!(
    fn ecam_is_mapped_uncached_and_reads_what_enumeration_found() {
        use lumen_mm::paging::Cache;

        use crate::kassert_eq;
        let mut guard = ECAM.lock();
        let ecam = guard.as_mut().ok_or("no ECAM")?;
        for d in devices() {
            let id = ecam.read(d.addr, 0);
            kassert_eq!((id as u16, (id >> 16) as u16), (d.vendor, d.device));
        }
        let host = PciAddr::new(0, 0, 0, 0).expect("valid");
        let virt = VirtAddr::new(ecam.dword(host, 0) as u64).ok_or("not canonical")?;
        let m = paging::walk(virt).mapping().ok_or("ECAM not mapped")?;
        kassert_eq!(m.phys_of(virt).map(|p| p.as_u64()), Some(0xe000_0000));
        kassert_eq!(m.cache(), Some(Cache::Uncached));
        // A function that doesn't exist reads as all ones.
        kassert_eq!(ecam.read(PciAddr::new(0, 0x80, 31, 7).expect("valid"), 0), u32::MAX);
        Ok(())
    }
);

crate::ktest!(
    fn the_registry_records_every_memory_bar_and_maps_each_once() {
        use lumen_mm::paging::Cache;

        use crate::{kassert, kassert_eq};
        let memory_bars = devices().iter().flat_map(|d| d.bars.iter()).flatten().filter(|b| b.is_memory()).count();
        let entries: Vec<BarEntry> = REGISTRY.lock().clone();
        kassert_eq!(entries.len(), memory_bars);
        kassert!(memory_bars > 0);
        kassert!(entries.iter().all(|e| e.mapped.is_none()));
        // A BAR with no page in the HHDM (the VGA BAR holding the
        // framebuffer has some: `map_mmio` refuses it): mapped once,
        // uncached, to its physical address.
        let outside_hhdm = |e: &BarEntry| {
            (e.phys..e.phys + e.size)
                .step_by(lumen_mm::FRAME_SIZE as usize)
                .all(|p| PhysAddr::new(p).is_some_and(|p| !paging::hhdm_covers(p, 1)))
        };
        let e = entries.iter().find(|e| outside_hhdm(e)).copied().ok_or("no memory BAR outside the HHDM")?;
        let err = |why| Err(BarError { addr: e.addr, index: e.index, why });
        kassert_eq!(mapped_bar(e.addr, e.index), err(BarWhy::NotMapped));
        let m = map_bar(e.addr, e.index).map_err(|err| {
            crate::ktest::diag(format_args!("{err}"));
            "mapping failed"
        })?;
        kassert_eq!((m.phys().as_u64(), m.size()), (e.phys, e.size));
        kassert_eq!(map_bar(e.addr, e.index), Ok(m));
        kassert_eq!(mapped_bar(e.addr, e.index), Ok(m));
        kassert_eq!(REGISTRY.lock().iter().filter(|e| e.mapped.is_some()).count(), 1);
        let walk = paging::walk(m.virt()).mapping().ok_or("not mapped")?;
        kassert_eq!(walk.phys_of(m.virt()), Some(m.phys()));
        kassert_eq!(walk.cache(), Some(Cache::Uncached));
        // What isn't a memory BAR is refused, saying why.
        let fail = |addr, index, why| Err(BarError { addr, index, why });
        kassert_eq!(map_bar(e.addr, 6), fail(e.addr, 6, BarWhy::OutOfRange));
        let nowhere = PciAddr::new(0, 0x80, 31, 7).expect("valid");
        kassert_eq!(map_bar(nowhere, 0), fail(nowhere, 0, BarWhy::NoSuchFunction));
        // q35's ICH9 SATA controller: BAR 4 is I/O, BAR 0 isn't implemented
        // (AHCI mode).
        let sata = PciAddr::new(0, 0, 0x1f, 2).expect("valid");
        kassert_eq!(map_bar(sata, 4), fail(sata, 4, BarWhy::Io));
        kassert_eq!(map_bar(sata, 0), fail(sata, 0, BarWhy::NotImplemented));
        kassert_eq!(
            alloc::format!("{}", BarError { addr: sata, index: 4, why: BarWhy::Io }),
            "0000:00:1f.2 BAR 4: an I/O BAR"
        );
        Ok(())
    }
);

crate::ktest!(
    fn the_status_says_why_there_are_no_devices() {
        use crate::{kassert, kassert_eq};
        let show = |missing, skipped| alloc::format!("{}", Status { missing, skipped });
        kassert_eq!(show(None, 0), "");
        kassert_eq!(show(None, 1), " (1 ECAM region skipped (refused or not mapped): see the boot log)");
        kassert_eq!(show(None, 3), " (3 ECAM regions skipped (refused or not mapped): see the boot log)");
        kassert_eq!(show(Some(NoPci::NoAcpi), 0), " (no PCI: running without ACPI, so no MCFG)");
        kassert_eq!(
            show(Some(NoPci::NoRegions), 2),
            " (no PCI: the MCFG lists no usable ECAM region; 2 ECAM regions skipped (refused or not mapped): see the \
             boot log)"
        );
        kassert_eq!(
            show(Some(NoPci::NoneMapped), 1),
            " (no PCI: no ECAM region could be mapped; 1 ECAM region skipped (refused or not mapped): see the boot log)"
        );
        let no_mcfg = NoPci::NoMcfg(acpi::Failure::without_cause(lumen_acpi::Error::NotFound {
            sig: lumen_acpi::Signature::MCFG,
            skipped: None,
        }));
        kassert_eq!(show(Some(no_mcfg), 0), " (no PCI: no MCFG table)");
        kassert!(Status { missing: None, skipped: 0 }.is_clean());
        kassert!(!Status { missing: None, skipped: 1 }.is_clean());
        Ok(())
    }
);
