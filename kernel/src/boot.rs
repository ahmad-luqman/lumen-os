//! Limine boot protocol requests.
//!
//! Limine scans the kernel image for these structures (between the start and
//! end markers) and fills in their responses before jumping to `_start`.
//! Everything the kernel knows about the machine at boot comes from here.

use core::cell::UnsafeCell;
use core::fmt;

use limine::BaseRevision;
use limine::memory_map::EntryType;
use limine::request::{
    ExecutableAddressRequest, ExecutableCmdlineRequest, ExecutableFileRequest, FramebufferRequest, HhdmRequest,
    MemoryMapRequest, MpRequest, RequestsEndMarker, RequestsStartMarker, RsdpRequest, StackSizeRequest,
};

/// Boot stack size we ask Limine for.
pub const BOOT_STACK_SIZE: u64 = 128 * 1024;
/// The stack Limine guarantees when it ignores our size request.
pub const LIMINE_DEFAULT_STACK_SIZE: u64 = 64 * 1024;

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static BASE_REVISION: BaseRevision = BaseRevision::with_revision(3);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static STACK_SIZE: StackSizeRequest = StackSizeRequest::new().with_size(BOOT_STACK_SIZE);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static MEMMAP: MemoryMapRequest = MemoryMapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static HHDM: HhdmRequest = HhdmRequest::new();

/// The kernel's own ELF file, used to symbolize crash reports.
#[used]
#[unsafe(link_section = ".limine_requests")]
pub static EXECUTABLE_FILE: ExecutableFileRequest = ExecutableFileRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static EXECUTABLE_ADDRESS: ExecutableAddressRequest = ExecutableAddressRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static CMDLINE: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

/// ACPI root pointer (reserved by the frame allocator; parsed by `acpi::init`).
#[used]
#[unsafe(link_section = ".limine_requests")]
pub static RSDP: RsdpRequest = RsdpRequest::new();

/// The other CPUs: Limine starts them and parks each until `smp` hands it an
/// entry point (xAPIC mode: no X2APIC flag).
#[used]
#[unsafe(link_section = ".limine_requests")]
pub static MP: MpRequest = MpRequest::new();

/// The TSC's frequency as Limine calibrated it: what its bootloader times
/// are in.
#[used]
#[unsafe(link_section = ".limine_requests")]
static TSC_FREQUENCY: RawRequest<TscFrequencyResponse> = RawRequest::new([0x10f2ee1d87d195e4, 0xf747a2b78f6ddb31]);

/// When Limine started and when it handed off to the kernel.
#[used]
#[unsafe(link_section = ".limine_requests")]
static PERFORMANCE: RawRequest<PerformanceResponse> = RawRequest::new([0x6b50ad9bf36d13ad, 0xdc4c7e88fc759e17]);

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

/// Name for a Limine memory-map entry type.
pub fn memmap_type_name(kind: EntryType) -> &'static str {
    match kind {
        EntryType::USABLE => "usable",
        EntryType::RESERVED => "reserved",
        EntryType::ACPI_RECLAIMABLE => "acpi-reclaimable",
        EntryType::ACPI_NVS => "acpi-nvs",
        EntryType::BAD_MEMORY => "bad",
        EntryType::BOOTLOADER_RECLAIMABLE => "bootloader-reclaimable",
        EntryType::EXECUTABLE_AND_MODULES => "kernel-and-modules",
        EntryType::FRAMEBUFFER => "framebuffer",
        _ => "unknown",
    }
}

/// A request the pinned `limine` crate (0.5) has no type for, in the
/// protocol's common layout (`limine.h`): Limine's common magic and the
/// request's two IDs, the revision we ask for (0), and the response pointer
/// Limine fills in. `R` is the response's layout.
#[repr(C)]
struct RawRequest<R> {
    id: [u64; 4],
    revision: u64,
    response: UnsafeCell<*const R>,
}

// SAFETY: Limine writes `response` before the kernel starts; the kernel
// only reads it.
unsafe impl<R> Sync for RawRequest<R> {}

impl<R> RawRequest<R> {
    const fn new(id: [u64; 2]) -> Self {
        Self {
            id: [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, id[0], id[1]],
            revision: 0,
            response: UnsafeCell::new(core::ptr::null()),
        }
    }

    fn response(&self) -> Option<&'static R> {
        // SAFETY: a volatile read, as Limine wrote the pointer behind the
        // compiler's back (the `limine` crate reads its own the same way).
        let ptr = unsafe { core::ptr::read_volatile(self.response.get()) };
        // SAFETY: a non-null pointer is Limine's response: an `R` (its
        // layout, from `limine.h`), aligned, in bootloader-reclaimable memory
        // that the frame allocator keeps reserved and the HHDM keeps mapped,
        // never written after the handoff.
        unsafe { ptr.as_ref() }
    }
}

/// `struct limine_tsc_frequency_response`.
#[repr(C)]
struct TscFrequencyResponse {
    _revision: u64,
    frequency: u64,
}

/// `struct limine_bootloader_performance_response` (the system reset's
/// time is always 0 from Limine, so unused).
#[repr(C)]
struct PerformanceResponse {
    _revision: u64,
    _reset_usec: u64,
    init_usec: u64,
    exec_usec: u64,
}

/// Limine's own times for the boot, in microseconds of the TSC (since its
/// reset) as Limine calibrated it (`hz`): when it started (just after the
/// firmware handed it control) and when it handed off to the kernel (its
/// last TSC reading before jumping to `_start`).
#[derive(Clone, Copy, Debug)]
pub struct LimineTimes {
    pub hz: u64,
    pub started_us: u64,
    pub handed_off_us: u64,
}

impl LimineTimes {
    /// Both responses, or why not: Limine gives neither without a TSC
    /// frequency.
    pub fn get() -> Result<LimineTimes, &'static str> {
        let tsc = TSC_FREQUENCY.response().ok_or("no TSC frequency from Limine")?;
        let perf = PERFORMANCE.response().ok_or("no bootloader performance times from Limine")?;
        Ok(LimineTimes { hz: tsc.frequency, started_us: perf.init_usec, handed_off_us: perf.exec_usec })
    }

    /// The TSC at the handoff, at or before Limine's own reading: the
    /// earliest the kernel's first instruction can have run. Exact in ticks
    /// (Limine's microseconds turned back with its own frequency), so
    /// independent of how fast the boot went.
    pub fn handoff_tsc(&self) -> Option<u64> {
        lumen_time::ticks_at_usec(self.handed_off_us, self.hz)
    }
}

/// How the boot timeline's origin (`kmain`'s first TSC reading, `entry`)
/// relates to Limine's times: `limine started at 1834.112 ms and handed off
/// at 2210.405 ms (TSC time), kmain began +0.031 ms after the handoff`. The
/// gap is measured in ticks, so its sign is exact; a negative one (an origin
/// before the handoff: the TSC's reset or the firmware's handoff to Limine)
/// reads `-...`.
pub struct BeforeKmain {
    pub limine: LimineTimes,
    pub entry: u64,
    pub clock: lumen_time::Clock,
}

impl BeforeKmain {
    /// Limine started after the TSC's reset and before its handoff, and the
    /// origin is at or after the handoff.
    pub fn in_order(&self) -> bool {
        let l = &self.limine;
        0 < l.started_us && l.started_us < l.handed_off_us && l.handoff_tsc().is_some_and(|h| h <= self.entry)
    }
}

impl fmt::Display for BeforeKmain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use lumen_time::Millis;
        let l = &self.limine;
        let ms = |us: u64| Millis(us.saturating_mul(1000));
        write!(
            f,
            "limine started at {} and handed off at {} (TSC time), kmain began ",
            ms(l.started_us),
            ms(l.handed_off_us)
        )?;
        let Some(handoff) = l.handoff_tsc() else {
            return f.write_str("at an unknown time after the handoff (it doesn't fit in 64 bits)");
        };
        match self.entry.checked_sub(handoff) {
            Some(after) => write!(f, "{:+} after the handoff", Millis(self.clock.ns(after))),
            None => write!(f, "-{} after the handoff", Millis(self.clock.ns(handoff - self.entry))),
        }
    }
}
