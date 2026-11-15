//! PCI discovery: every function in the MCFG's configuration space, read
//! once at boot into [`devices`] (docs/ARCHITECTURE.md, section 7).
//!
//! Configuration space is reached through ECAM only (the legacy `0xcf8`
//! ports aren't used): each MCFG region's buses are mapped once, uncached,
//! with `map_mmio` (1 MiB per bus: 256 MiB for q35's 256 buses, which OVMF
//! puts at 0xe0000000), behind an
//! accessor private to this module. `lumen_pci` decodes headers, sizes BARs
//! and walks capability lists; the scan is brute force (every bus in the
//! range, devices 0-31, functions 1-7 only for multi-function devices), so
//! bridges need no recursion. No ACPI, no MCFG or an MCFG without a usable
//! region means no PCI devices, with a warning, not a crash.
//!
//! Enumeration also records every memory BAR in the BAR registry. BARs are
//! mapped only by the registry (`map_bar`), once each, when a driver claims
//! the device (#95); nothing else maps them, and the mappings are never
//! undone (`map_mmio`'s window only grows).
//!
//! Lock order: the registry, then the page tables (`map_mmio`) and the
//! frame allocator under them.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt;
use core::ops::RangeInclusive;

use log::{debug, info, warn};
use lumen_acpi::EcamRegion;
use lumen_mm::{PhysAddr, VirtAddr};
pub use lumen_pci::{Bar, CapEnd, DeviceInfo, PciAddr, names};
use lumen_pci::{ConfigSpace, enumerate};

use crate::acpi;
use crate::mm::paging::{self, MmioError};
use crate::sync::SpinLock;

/// One MCFG region, mapped.
struct Region {
    segment: u16,
    buses: RangeInclusive<u8>,
    /// The physical and virtual address of the first bus's configuration
    /// space.
    phys: u64,
    virt: u64,
}

/// Configuration space through ECAM: the mapped regions. Reads and writes
/// of a function outside them are a bug (every address comes from the scan
/// of these regions) and panic.
struct Ecam {
    regions: Vec<Region>,
}

impl Ecam {
    /// The dword at `offset` in `addr`'s configuration space.
    fn dword(&self, addr: PciAddr, offset: u16) -> *mut u32 {
        assert!(offset.is_multiple_of(4) && offset < 4096, "PCI config offset {offset:#x} for {addr}");
        let r = self
            .regions
            .iter()
            .find(|r| r.segment == addr.segment() && r.buses.contains(&addr.bus()))
            .unwrap_or_else(|| panic!("{addr} is outside every ECAM region"));
        let bus = u64::from(addr.bus() - r.buses.start());
        (r.virt + (bus << 20) + u64::from(addr.ecam_offset()) + u64::from(offset)) as *mut u32
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

/// Whether [`init`] has run, and the functions it found.
#[derive(Clone, Copy)]
enum Discovery {
    NotYet,
    Done(&'static [DeviceInfo]),
}

static DISCOVERY: SpinLock<Discovery> = SpinLock::new(Discovery::NotYet);

/// The mapped configuration space, for #95's claim and its function
/// config (nothing outside this module reaches it).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by ktests until #95
static ECAM: SpinLock<Option<Ecam>> = SpinLock::new(None);

/// Every PCI function found at boot, in address order; empty without
/// ECAM. Never changes after [`init`]. Panics before it (a boot-order bug).
pub fn devices() -> &'static [DeviceInfo] {
    match *DISCOVERY.lock() {
        Discovery::NotYet => panic!("pci::devices called before pci::init"),
        Discovery::Done(devices) => devices,
    }
}

/// Map each MCFG region, scan it, record the memory BARs and log what was
/// found. After `acpi::init` (and `mm::init`), before any driver, and
/// while nothing uses a device: BAR sizing briefly turns each device's
/// decoding off (it runs before the framebuffer's terminal draws). Panics
/// if called twice.
pub fn init() {
    {
        let mut d = DISCOVERY.lock();
        assert!(matches!(*d, Discovery::NotYet), "pci::init called twice");
        *d = Discovery::Done(&[]);
    }
    let regions = match acpi::info().map(|info| &info.ecam) {
        None => return warn!(target: "pci", "no PCI devices: no ACPI tables, so no MCFG"),
        Some(Err(e)) => return warn!(target: "pci", "no PCI devices: {e}"),
        Some(Ok(regions)) if regions.is_empty() => {
            return warn!(target: "pci", "no PCI devices: the MCFG lists no ECAM region");
        }
        Some(Ok(regions)) => regions,
    };
    let mut ecam = Ecam { regions: regions.iter().filter_map(map_region).collect() };
    if ecam.regions.is_empty() {
        return warn!(target: "pci", "no PCI devices: no ECAM region could be mapped");
    }
    let mut found = Vec::new();
    for i in 0..ecam.regions.len() {
        let (segment, buses) = (ecam.regions[i].segment, ecam.regions[i].buses.clone());
        enumerate(&mut ecam, segment, buses, |d| found.push(d));
    }
    found.sort_unstable_by_key(|d| d.addr);
    for d in &found {
        log_device(d);
    }
    let bars: Vec<BarEntry> = found
        .iter()
        .flat_map(|d| d.bars.iter().enumerate().map(move |(i, bar)| (d.addr, i, bar)))
        .filter_map(|(addr, index, bar)| match *bar {
            Some(Bar::Memory { phys, size, .. }) => Some(BarEntry { addr, index, phys, size, mapped: None }),
            _ => None,
        })
        .collect();
    let plural = if found.len() == 1 { "" } else { "s" };
    info!(target: "pci", "{} function{plural}, {} memory BARs, through ECAM: {}", found.len(), bars.len(), Regions(&ecam));
    *REGISTRY.lock() = bars;
    *ECAM.lock() = Some(ecam);
    *DISCOVERY.lock() = Discovery::Done(Box::leak(found.into_boxed_slice()));
}

/// Map `r`'s buses, or warn and skip it.
fn map_region(r: &EcamRegion) -> Option<Region> {
    let buses = r.bus_start..=r.bus_end;
    let mapped = PhysAddr::new(r.start()).ok_or(MmioError::BadRange).and_then(|p| paging::map_mmio(p, r.len()));
    match mapped {
        Ok(virt) => Some(Region { segment: r.segment, buses, phys: r.start(), virt: virt.as_u64() }),
        Err(e) => {
            warn!(
                target: "pci",
                "segment {} buses {}-{} skipped: mapping its ECAM at {:#x}: {e}",
                r.segment,
                r.bus_start,
                r.bus_end,
                r.start()
            );
            None
        }
    }
}

/// `segment 0 buses 0-255 at 0xb0000000`, for each mapped region.
struct Regions<'a>(&'a Ecam);

impl fmt::Display for Regions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, r) in self.0.regions.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            write!(f, "{sep}segment {} buses {}-{} at {:#x}", r.segment, r.buses.start(), r.buses.end(), r.phys)?;
        }
        Ok(())
    }
}

/// One debug line per function; a warning for a capability list that was
/// cut off (that device is never probed).
fn log_device(d: &DeviceInfo) {
    let name = names::device(d.vendor, d.device, d.subsystem.1).unwrap_or("");
    let class = names::class(d.class, d.subclass);
    debug!(
        target: "pci",
        "{} {:04x}:{:04x} {class} ({:02x}{:02x}{:02x}) {name}",
        d.addr,
        d.vendor,
        d.device,
        d.class,
        d.subclass,
        d.prog_if
    );
    match d.caps.end() {
        CapEnd::Complete => {}
        CapEnd::IntoHeader { from, to } => warn!(
            target: "pci",
            "{}: capability list cut off: {from:#x} points into the header ({to:#x}); it won't be probed",
            d.addr
        ),
        CapEnd::TooLong => warn!(
            target: "pci",
            "{}: capability list cut off after {} entries (it loops); it won't be probed",
            d.addr,
            lumen_pci::MAX_CAPS
        ),
    }
}

/// A memory BAR enumeration found, and its mapping once claimed.
#[derive(Clone, Copy, Debug)]
struct BarEntry {
    addr: PciAddr,
    index: usize,
    phys: u64,
    size: u64,
    mapped: Option<VirtAddr>,
}

/// The BAR registry: every memory BAR, in address order. The one record
/// of BAR mappings.
static REGISTRY: SpinLock<Vec<BarEntry>> = SpinLock::new(Vec::new());

/// Why a BAR couldn't be mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: #95's claim
pub enum BarError {
    /// Enumeration recorded no memory BAR `index` for that function.
    NoSuchBar,
    Mmio(MmioError),
}

impl fmt::Display for BarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BarError::NoSuchBar => f.write_str("no such memory BAR"),
            BarError::Mmio(e) => write!(f, "{e}"),
        }
    }
}

/// Map memory BAR `index` of `addr` (uncached), the first time it is asked
/// for; later calls return the same mapping. The registry's lock is held
/// across `map_mmio`, so two callers can't map one BAR twice.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first caller: #95's claim
fn map_bar(addr: PciAddr, index: usize) -> Result<VirtAddr, BarError> {
    let mut registry = REGISTRY.lock();
    let entry = registry.iter_mut().find(|e| e.addr == addr && e.index == index).ok_or(BarError::NoSuchBar)?;
    if let Some(virt) = entry.mapped {
        return Ok(virt);
    }
    let phys = PhysAddr::new(entry.phys).ok_or(BarError::Mmio(MmioError::BadRange))?;
    let virt = paging::map_mmio(phys, entry.size).map_err(BarError::Mmio)?;
    entry.mapped = Some(virt);
    Ok(virt)
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
        kassert!(devices().iter().all(|d| d.caps.is_complete()));
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
        let base = acpi::info().and_then(|i| i.ecam.as_ref().ok()).and_then(|e| e.first()).ok_or("no MCFG")?.base;
        kassert_eq!(m.phys_of(virt).map(|p| p.as_u64()), Some(base));
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
        // A BAR with no page in the HHDM (the VGA BAR holding the
        // framebuffer has some: `map_mmio` refuses it), not mapped yet:
        // mapped once, uncached, to its physical address.
        let outside_hhdm = |e: &BarEntry| {
            (e.phys..e.phys + e.size)
                .step_by(lumen_mm::FRAME_SIZE as usize)
                .all(|p| PhysAddr::new(p).is_some_and(|p| !paging::hhdm_covers(p, 1)))
        };
        let e = entries
            .iter()
            .find(|e| e.mapped.is_none() && outside_hhdm(e))
            .copied()
            .ok_or("no unmapped memory BAR outside the HHDM")?;
        let virt = map_bar(e.addr, e.index).map_err(|err| {
            crate::ktest::diag(format_args!(
                "{} BAR {} at {:#x} ({:#x} bytes): {err}",
                e.addr, e.index, e.phys, e.size
            ));
            "mapping failed"
        })?;
        kassert_eq!(map_bar(e.addr, e.index), Ok(virt));
        let m = paging::walk(virt).mapping().ok_or("not mapped")?;
        kassert_eq!(m.phys_of(virt).map(|p| p.as_u64()), Some(e.phys));
        kassert_eq!(m.cache(), Some(Cache::Uncached));
        kassert_eq!(map_bar(e.addr, 6), Err(BarError::NoSuchBar));
        Ok(())
    }
);
