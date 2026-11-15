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
//! Enumeration also fills the registry: one slot per function, its memory
//! BARs and who has it (`Binding`, which `lspci`'s driver column shows).
//! The registry maps a BAR (`map_bar`) only when asked, once, when a driver
//! claims the device ([`claim`]), and never unmaps it (`map_mmio`'s window
//! only grows). Nothing else maps BARs, except that the framebuffer Limine
//! set up inside the VGA BAR is in the HHDM (so `map_mmio` refuses that
//! BAR). A claim hands out [`DeviceResources`]: the function's BAR mappings
//! and its own configuration space ([`FunctionConfig`]), nothing else.
//!
//! Locks: `DISCOVERY` is held only to copy it out, and released before any
//! other is taken. `REGISTRY` is taken before the page tables and the
//! frame allocator (`map_bar` holds it across `map_mmio`) and before the
//! heap (a refusal's reason and the binding's text are `String`s, cloned
//! under it); `ECAM` is taken last (after virtio's probe lock while
//! probing), except in the ECAM ktests (before `DISCOVERY`).

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use log::{debug, info, warn};
use lumen_acpi::EcamRegion;
use lumen_mm::{PhysAddr, VirtAddr};
pub use lumen_pci::{Bar, BarProblem, DeviceInfo, NoBar, PciAddr};
use lumen_pci::{ConfigSpace, command, ecam_offset, enumerate};
use virtio_drivers::transport::pci::bus::{ConfigurationAccess, DeviceFunction};

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
        // space: BAR sizing and command registers at enumeration, the
        // claim's command register (and its restore), `probed`'s (bus
        // mastering off, or the restore after a decline), `ktest_command`,
        // and a claimed function's own space through its `FunctionConfig`
        // (`virtio-drivers` sizing its BARs while probing; a transport that
        // doesn't reset turning bus mastering off).
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

/// The mapped configuration space, for the claim and each claimed
/// function's [`FunctionConfig`] (nothing outside this module reaches it).
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

/// `addr`'s entry in [`devices`], if enumeration found it.
fn device(addr: PciAddr) -> Option<&'static DeviceInfo> {
    let devices = devices();
    devices.binary_search_by_key(&addr, |d| d.addr).ok().map(|i| &devices[i])
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
    let mut slots = Vec::with_capacity(found.len());
    for d in &found {
        debug!(target: "pci", "{d}");
        for problem in d.problems(|segment, bus| ecam.covers(segment, bus)) {
            warn!(target: "pci", "{}: {problem}", d.addr);
        }
        let bars = d.bars.map(|bar| match bar {
            Some(Bar::Memory { phys, size, .. }) => Some(BarEntry { phys, size, mapped: None }),
            _ => None,
        });
        slots.push(Slot { addr: d.addr, bars, state: SlotState::Free { declined_by: None }, resources_live: false });
    }
    let memory_bars = slots.iter().map(|s| s.bars.iter().flatten().count()).sum::<usize>();
    let plural = if found.len() == 1 { "" } else { "s" };
    info!(
        target: "pci",
        "{} function{plural}, {memory_bars} memory BARs, through ECAM: {}",
        found.len(),
        Regions(&ecam)
    );
    *REGISTRY.lock() = slots;
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
    phys: u64,
    size: u64,
    mapped: Option<BarMapping>,
}

/// One function's entry in the registry: its memory BARs (by index), who
/// has it, and whether the [`DeviceResources`] a claim handed out are still
/// alive.
#[derive(Clone, Debug)]
struct Slot {
    addr: PciAddr,
    bars: [Option<BarEntry>; 6],
    state: SlotState,
    resources_live: bool,
}

/// Who has a function, with what only the registry needs (the command
/// register to restore if the probing driver declines it). [`Binding`] is
/// its public view.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SlotState {
    /// Never claimed, or freed again: the last driver that declined it.
    Free {
        declined_by: Option<&'static str>,
    },
    NotProbed(&'static str, String),
    Probing {
        driver: &'static str,
        restore: u16,
    },
    Bound(&'static str),
    Failed(&'static str, String),
}

impl SlotState {
    fn binding(&self) -> Binding {
        match self {
            SlotState::Free { declined_by } => Binding::Free { declined_by: *declined_by },
            SlotState::NotProbed(driver, why) => Binding::NotProbed(driver, why.clone()),
            SlotState::Probing { driver, .. } => Binding::Probing(driver),
            SlotState::Bound(driver) => Binding::Bound(driver),
            SlotState::Failed(driver, why) => Binding::Failed(driver, why.clone()),
        }
    }
}

/// The registry: one slot per function, in [`devices`]' order. The one
/// record of BAR mappings and of claims.
static REGISTRY: SpinLock<Vec<Slot>> = SpinLock::new(Vec::new());

/// `f` on `addr`'s slot, under the registry's lock; `None` if enumeration
/// found no such function.
fn with_slot<R>(addr: PciAddr, f: impl FnOnce(&mut Slot) -> R) -> Option<R> {
    let i = devices().binary_search_by_key(&addr, |d| d.addr).ok()?;
    let mut registry = REGISTRY.lock();
    // (The registry's lock is taken after `DISCOVERY`'s is released.)
    let slot = registry.get_mut(i)?;
    debug_assert_eq!(slot.addr, addr, "the registry is in devices() order");
    Some(f(slot))
}

/// Who has a function: what `lspci`'s driver column shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// No driver has it: none matches it or was offered it
    /// (`declined_by: None`), or the last one offered it declined it.
    Free {
        declined_by: Option<&'static str>,
    },
    /// This driver matched it but the claim refused: why.
    NotProbed(&'static str, String),
    /// Claimed by this driver, which is probing it.
    Probing(&'static str),
    Bound(&'static str),
    /// The driver's probe failed: why.
    Failed(&'static str, String),
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Binding::Free { declined_by: None } => f.write_str("no driver"),
            Binding::Free { declined_by: Some(driver) } => write!(f, "no driver ({driver} declined it)"),
            Binding::NotProbed(driver, why) => write!(f, "{driver}: not probed: {why}"),
            Binding::Probing(driver) => write!(f, "{driver}: probing"),
            Binding::Bound(driver) => write!(f, "{driver}: bound"),
            Binding::Failed(driver, why) => write!(f, "{driver}: failed: {why}"),
        }
    }
}

/// Who has `addr` (for `lspci`); `None` if there is no such function.
pub fn binding(addr: PciAddr) -> Option<Binding> {
    with_slot(addr, |s| s.state.binding())
}

/// A memory BAR's mapping: uncached, for the kernel's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarMapping {
    phys: PhysAddr,
    virt: VirtAddr,
    size: u64,
}

impl BarMapping {
    pub fn phys(&self) -> PhysAddr {
        self.phys
    }

    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: rng (#97) or #96's MSI-X table
    pub fn virt(&self) -> VirtAddr {
        self.virt
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Where `[phys, phys + len)` is mapped, if it lies inside this BAR.
    pub fn virt_of(&self, phys: u64, len: u64) -> Option<VirtAddr> {
        let offset = phys.checked_sub(self.phys.as_u64())?;
        if offset.checked_add(len)? > self.size {
            return None;
        }
        VirtAddr::new(self.virt.as_u64() + offset)
    }
}

/// Why BAR `index` of `addr` has no mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarError {
    pub addr: PciAddr,
    pub index: usize,
    pub why: BarWhy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    let Some(d) = device(addr) else { return fail(BarWhy::NoSuchFunction) };
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
fn map_bar(addr: PciAddr, index: usize) -> Result<BarMapping, BarError> {
    check_bar(addr, index)?;
    let mapped = with_slot(addr, |slot| {
        let e = slot.bars[index].as_mut().expect("every memory BAR is registered");
        if let Some(m) = e.mapped {
            return Ok(m);
        }
        let (raw, size) = (e.phys, e.size);
        let fail = |error| BarError { addr, index, why: BarWhy::Mmio { phys: raw, size, error } };
        let phys = PhysAddr::new(raw).ok_or(fail(MmioError::BadRange))?;
        let virt = paging::map_mmio(phys, size).map_err(fail)?;
        Ok(*e.mapped.insert(BarMapping { phys, virt, size }))
    });
    mapped.expect("check_bar found the function")
}

/// Memory BAR `index` of `addr`'s mapping, if [`map_bar`] made one; maps
/// nothing (for the virtio adapter's pre-probe check and
/// `mmio_phys_to_virt`, and #96's MSI-X table).
fn mapped_bar(addr: PciAddr, index: usize) -> Result<BarMapping, BarError> {
    check_bar(addr, index)?;
    let mapped = with_slot(addr, |slot| slot.bars[index].expect("every memory BAR is registered").mapped);
    mapped.expect("check_bar found the function").ok_or(BarError { addr, index, why: BarWhy::NotMapped })
}

/// Why [`claim`] refused a function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimError {
    /// Enumeration found no such function.
    NoSuchFunction,
    /// It is claimed already, or a claim of it was refused before (by
    /// `probe_all` or a ktest): its binding, which this claim leaves as it
    /// was.
    Taken(Binding),
    /// Its capability list was cut off or not read.
    Caps(lumen_pci::CapEnd),
    /// A memory BAR couldn't be mapped (or is unassigned or malformed).
    Bar(BarError),
    /// The command register didn't keep what the claim wrote (memory space
    /// and bus master on, I/O space off): what it reads instead.
    Command { wrote: u16, read: u16 },
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClaimError::NoSuchFunction => f.write_str("no such function"),
            ClaimError::Taken(b) => write!(f, "already claimed ({b})"),
            ClaimError::Caps(end) => write!(f, "capability list {end}"),
            ClaimError::Bar(e) => write!(f, "{e}"),
            ClaimError::Command { wrote, read } => {
                write!(f, "the command register reads {read:#06x} after {wrote:#06x} was written")
            }
        }
    }
}

/// The command bits a claim sets (memory space, bus master) and clears
/// (I/O space).
const CLAIM_ON: u16 = command::MEMORY_SPACE | command::BUS_MASTER;
const CLAIM_OFF: u16 = command::IO_SPACE;

/// Write `value` to `addr`'s command register and read it back; a
/// mismatch is a warning naming `what` was meant. Returns what it reads.
fn set_command(addr: PciAddr, value: u16, what: &str) -> u16 {
    let read = with_ecam(|ecam| {
        ecam.write_command(addr, value);
        ecam.read16(addr, lumen_pci::reg::COMMAND)
    });
    if read != value {
        warn!(target: "pci", "{addr}: the command register reads {read:#06x} after {value:#06x} was written ({what})");
    }
    read
}

/// Give `addr` to `driver`: once per claim (a second claim is
/// [`ClaimError::Taken`], whoever asks; a function is claimed again only
/// after its driver declined it), recorded in the registry as
/// [`Binding::Probing`]. Maps every memory BAR (an I/O BAR, unassigned or
/// not, unless malformed, and an unimplemented slot are skipped; an
/// unassigned or malformed memory BAR, or a malformed I/O BAR, refuses),
/// then turns memory space and bus master on and I/O space off in the
/// command register (`virtio-drivers` never sets bus master, and the
/// firmware's state isn't trusted), reading it back. A function whose
/// capability list was cut off or not read, whose BAR can't be mapped, or
/// whose command register doesn't keep those bits (then restored to what
/// it was) is refused and recorded as [`Binding::NotProbed`] by `driver`,
/// with the reason.
///
/// Only `drivers::probe_all` calls this at boot (and ktests, on devices no
/// driver takes).
pub fn claim(addr: PciAddr, driver: &'static str) -> Result<DeviceResources, ClaimError> {
    let Some(info) = device(addr) else { return Err(ClaimError::NoSuchFunction) };
    // Read before taking the slot (ECAM isn't taken under the registry's
    // lock): only the claim that takes the slot writes it afterwards.
    let before = with_ecam(|e| e.read16(addr, lumen_pci::reg::COMMAND));
    with_slot(addr, |slot| match slot.state {
        SlotState::Free { .. } => {
            slot.state = SlotState::Probing { driver, restore: before };
            Ok(())
        }
        ref other => Err(ClaimError::Taken(other.binding())),
    })
    .expect("found in devices()")?;
    let refuse = |e: ClaimError| {
        with_slot(addr, |slot| slot.state = SlotState::NotProbed(driver, e.to_string()));
        Err(e)
    };
    if !info.probeable() {
        return refuse(ClaimError::Caps(info.caps.end()));
    }
    for (index, bar) in info.bars.iter().enumerate() {
        match bar {
            None | Some(Bar::Io { .. } | Bar::Unassigned { io: true, .. }) => {}
            Some(_) => {
                if let Err(e) = map_bar(addr, index) {
                    return refuse(ClaimError::Bar(e));
                }
            }
        }
    }
    let wrote = (before | CLAIM_ON) & !CLAIM_OFF;
    let read = with_ecam(|ecam| {
        ecam.write_command(addr, wrote);
        ecam.read16(addr, lumen_pci::reg::COMMAND)
    });
    debug!(target: "pci", "{addr}: claimed by {driver}, command {before:#06x} -> {read:#06x}");
    if read & (CLAIM_ON | CLAIM_OFF) != CLAIM_ON {
        set_command(addr, before, "restoring it after a refused claim");
        return refuse(ClaimError::Command { wrote, read });
    }
    with_slot(addr, |slot| slot.resources_live = true);
    Ok(DeviceResources { info })
}

/// How a driver's probe of the function it claimed went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probed {
    Bound,
    /// It failed: why. Bus mastering is turned off, so the device can't
    /// reach memory its driver dropped.
    Failed(String),
    /// Not this driver's device after all (one its `matches` couldn't rule
    /// out, e.g. a second tablet, or a virtio input device of the tablet's
    /// class without its axes): the function is free again, its command
    /// register as before the claim, for the next driver row. Refused (a
    /// failure) if the driver kept its `DeviceResources`.
    Declined,
}

/// A bound function's driver gave up on it later (its thread saw the
/// device fail): `lspci` shows `<driver>: failed: <why>` from now on, and
/// bus mastering is turned off. False, changing nothing, unless a driver
/// has `addr` bound.
pub fn failed_after_bind(addr: PciAddr, why: String) -> bool {
    let driver = with_slot(addr, |slot| match slot.state {
        SlotState::Bound(driver) => Some(driver),
        _ => None,
    })
    .flatten();
    let Some(driver) = driver else { return false };
    let now = with_ecam(|ecam| ecam.read16(addr, lumen_pci::reg::COMMAND));
    set_command(addr, now & !command::BUS_MASTER, "bus mastering off after its device failed");
    with_slot(addr, |slot| slot.state = SlotState::Failed(driver, why));
    true
}

/// Record how `driver`'s probe of the function it claimed went (see
/// [`Probed`]): the command register first (read back, a mismatch
/// warned about), then the binding, so no other claim can see the
/// function free before its register is restored. Panics unless `addr` is
/// being probed by `driver` (a bug in `probe_all`). Returns the binding
/// recorded.
pub fn probed(addr: PciAddr, driver: &'static str, outcome: Probed) -> Binding {
    let (restore, live) = with_slot(addr, |slot| match slot.state {
        SlotState::Probing { driver: d, restore } if d == driver => (restore, slot.resources_live),
        ref other => panic!("pci::probed: {addr} isn't being probed by {driver} ({})", other.binding()),
    })
    .unwrap_or_else(|| panic!("pci::probed: no function {addr}"));
    let bus_master_off = || {
        let now = with_ecam(|ecam| ecam.read16(addr, lumen_pci::reg::COMMAND));
        set_command(addr, now & !command::BUS_MASTER, "bus mastering off after a failed probe");
    };
    let state = match outcome {
        Probed::Bound => SlotState::Bound(driver),
        Probed::Failed(why) => {
            bus_master_off();
            SlotState::Failed(driver, why)
        }
        Probed::Declined if live => {
            warn!(target: "pci", "{addr}: {driver} declined it but kept its resources: recorded as failed");
            bus_master_off();
            SlotState::Failed(driver, String::from("declined the device but kept its resources"))
        }
        Probed::Declined => {
            set_command(addr, restore, "restoring it after a decline");
            SlotState::Free { declined_by: Some(driver) }
        }
    };
    let binding = state.binding();
    with_slot(addr, |slot| slot.state = state);
    binding
}

/// A claimed function's resources, and nothing else: its `DeviceInfo`, its
/// memory BARs' mappings and its own configuration space. Only [`claim`]
/// makes one (private fields, not `Clone`), once per claim; the registry
/// knows while it lives (a driver can't decline a device and keep it).
#[derive(Debug)]
pub struct DeviceResources {
    info: &'static DeviceInfo,
}

impl Drop for DeviceResources {
    fn drop(&mut self) {
        with_slot(self.info.addr, |slot| slot.resources_live = false);
    }
}

impl DeviceResources {
    pub fn info(&self) -> &'static DeviceInfo {
        self.info
    }

    /// Memory BAR `i`'s mapping (the claim mapped every memory BAR); `None`
    /// for an I/O BAR, an unimplemented slot, the upper half of a 64-bit
    /// BAR or `i` above 5.
    pub fn bar(&self, i: usize) -> Option<BarMapping> {
        match mapped_bar(self.info.addr, i) {
            Ok(m) => Some(m),
            Err(BarError { why: BarWhy::NotMapped, .. }) => panic!("{}: claimed, so BAR {i} is mapped", self.info.addr),
            Err(_) => None,
        }
    }

    /// This function's configuration space, and only its, for as long as
    /// these resources live.
    pub fn config(&self) -> FunctionConfig<'_> {
        FunctionConfig { info: self.info, claim: PhantomData }
    }
}

/// One claimed function's configuration space, tied to its
/// [`DeviceResources`]: reads and writes go through ECAM (its lock taken
/// for each), and another function's address is a bug that panics, as is
/// a write into its MSI or MSI-X capability (#96 programs those through a
/// path private to `pci`; `virtio-drivers` 0.13 writes only the command
/// register and the BARs, sizing them while probing, before the device
/// runs: no BAR emulation in M3).
#[derive(Debug)]
pub struct FunctionConfig<'a> {
    info: &'static DeviceInfo,
    claim: PhantomData<&'a DeviceResources>,
}

impl FunctionConfig<'_> {
    fn check(&self, addr: PciAddr) {
        assert_eq!(addr, self.info.addr, "{}'s config access used for {addr}", self.info.addr);
    }

    /// `df` as this function's address; panics if it is another function.
    fn same(&self, df: DeviceFunction) -> PciAddr {
        let a = self.info.addr;
        assert!(
            (df.bus, df.device, df.function) == (a.bus(), a.device(), a.function()),
            "{a}'s config access used for {:02x}:{:02x}.{:x}",
            df.bus,
            df.device,
            df.function
        );
        a
    }

    /// Write `value` at `offset` unless that is inside an MSI or MSI-X
    /// capability (panics: a driver bug).
    fn guarded_write(&self, addr: PciAddr, offset: u16, value: u32) {
        with_ecam(|e| {
            if let Some(cap) = self.info.caps.interrupt_cap_at(e, addr, offset) {
                panic!(
                    "{addr}: a driver wrote {value:#x} at {offset:#x}, inside the capability {:#04x} at {:#x}",
                    cap.id, cap.offset
                );
            }
            e.write(addr, offset, value)
        })
    }
}

/// `f` on ECAM, under its lock, for a claimed function's access.
fn with_ecam<R>(f: impl FnOnce(&mut Ecam) -> R) -> R {
    f(ECAM.lock().as_mut().expect("claimed, so ECAM is mapped"))
}

impl ConfigSpace for FunctionConfig<'_> {
    fn read(&mut self, addr: PciAddr, offset: u16) -> u32 {
        self.check(addr);
        with_ecam(|e| e.read(addr, offset))
    }

    fn write(&mut self, addr: PciAddr, offset: u16, value: u32) {
        self.check(addr);
        self.guarded_write(addr, offset, value)
    }
}

/// `virtio-drivers`' view: a bus/device/function without a segment, so the
/// segment is this function's, and anything but this function panics.
impl ConfigurationAccess for FunctionConfig<'_> {
    fn read_word(&self, df: DeviceFunction, register_offset: u8) -> u32 {
        let addr = self.same(df);
        with_ecam(|e| e.read(addr, u16::from(register_offset)))
    }

    fn write_word(&mut self, df: DeviceFunction, register_offset: u8, data: u32) {
        let addr = self.same(df);
        self.guarded_write(addr, u16::from(register_offset), data)
    }

    // The contract: a clone may only read read-only fields. Only the
    // crate's `enumerate_bus` calls this, which the kernel never uses;
    // every access takes ECAM's lock anyway, and the clone keeps the same
    // lifetime, so it can't outlive the claim.
    unsafe fn unsafe_clone(&self) -> Self {
        FunctionConfig { info: self.info, claim: PhantomData }
    }
}

/// Ktests: set `addr`'s command register directly (an adverse state before
/// a claim) and read it.
#[cfg(feature = "ktest")]
pub fn ktest_command(addr: PciAddr, set: Option<u16>) -> u16 {
    with_ecam(|e| {
        if let Some(value) = set {
            e.write_command(addr, value);
        }
        e.read16(addr, lumen_pci::reg::COMMAND)
    })
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
        let slots: Vec<Slot> = REGISTRY.lock().clone();
        kassert_eq!(slots.len(), devices().len());
        kassert!(slots.iter().zip(devices()).all(|(s, d)| s.addr == d.addr));
        let entries: Vec<(PciAddr, usize, BarEntry)> = slots
            .iter()
            .filter(|s| s.state == SlotState::Free { declined_by: None })
            .flat_map(|s| s.bars.iter().enumerate().filter_map(move |(i, e)| Some((s.addr, i, (*e)?))))
            .collect();
        kassert_eq!(slots.iter().map(|s| s.bars.iter().flatten().count()).sum::<usize>(), memory_bars);
        kassert!(memory_bars > 0);
        // A function never claimed has nothing mapped (the virtio ktests
        // claim theirs, and other ktests may run first; a declined one keeps
        // its BARs mapped).
        kassert!(entries.iter().all(|(_, _, e)| e.mapped.is_none()));
        // A BAR with no page in the HHDM (the VGA BAR holding the
        // framebuffer has some: `map_mmio` refuses it), of a function no
        // ktest claims: mapped once, uncached, to its physical address.
        let outside_hhdm = |e: &BarEntry| {
            (e.phys..e.phys + e.size)
                .step_by(lumen_mm::FRAME_SIZE as usize)
                .all(|p| PhysAddr::new(p).is_some_and(|p| !paging::hhdm_covers(p, 1)))
        };
        let not_virtio =
            |a: &PciAddr| devices().iter().any(|d| d.addr == *a && d.vendor != lumen_pci::names::VIRTIO_VENDOR);
        let (addr, index, e) = entries
            .iter()
            .find(|(a, _, e)| not_virtio(a) && outside_hhdm(e))
            .copied()
            .ok_or("no memory BAR outside the HHDM")?;
        let mapped_now =
            || REGISTRY.lock().iter().flat_map(|s| s.bars.iter().flatten()).filter(|e| e.mapped.is_some()).count();
        let before = mapped_now();
        let err = |why| Err(BarError { addr, index, why });
        kassert_eq!(mapped_bar(addr, index), err(BarWhy::NotMapped));
        let m = map_bar(addr, index).map_err(|err| {
            crate::ktest::diag(format_args!("{err}"));
            "mapping failed"
        })?;
        kassert_eq!((m.phys().as_u64(), m.size()), (e.phys, e.size));
        kassert_eq!(map_bar(addr, index), Ok(m));
        kassert_eq!(mapped_bar(addr, index), Ok(m));
        kassert_eq!(mapped_now(), before + 1);
        let walk = paging::walk(m.virt()).mapping().ok_or("not mapped")?;
        kassert_eq!(walk.phys_of(m.virt()), Some(m.phys()));
        kassert_eq!(walk.cache(), Some(Cache::Uncached));
        // What isn't a memory BAR is refused, saying why.
        let fail = |addr, index, why| Err(BarError { addr, index, why });
        kassert_eq!(map_bar(addr, 6), fail(addr, 6, BarWhy::OutOfRange));
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
