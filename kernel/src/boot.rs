//! Limine boot protocol requests.
//!
//! Limine scans the kernel image for these structures (between the start and
//! end markers) and fills in their responses before jumping to `_start`.
//! Everything the kernel knows about the machine at boot comes from here.

use core::cell::UnsafeCell;

use limine::BaseRevision;
use limine::memory_map::EntryType;
use limine::request::{
    ExecutableAddressRequest, ExecutableCmdlineRequest, ExecutableFileRequest, FramebufferRequest, HhdmRequest,
    MemoryMapRequest, MpRequest, RequestsEndMarker, RequestsStartMarker, RsdpRequest, StackSizeRequest,
};
use lumen_time::handoff::{BeforeKmain, LimineTimes, Unknown};

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
static TSC_FREQUENCY: RawRequest<TscFrequencyResponse> = RawRequest::new();

/// When Limine started and when it handed off to the kernel (the last TSC
/// reading it records before the jump).
#[used]
#[unsafe(link_section = ".limine_requests")]
static PERFORMANCE: RawRequest<PerformanceResponse> = RawRequest::new();

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

/// A Limine response the pinned `limine` crate (0.5) has no type for, tied
/// to its request's two IDs (after Limine's common magic).
///
/// These types (`RawRequest` and both responses) exist only because of that
/// pin: on a bump to `limine` 0.6.4 or later, delete them for its
/// `TscFrequencyRequest` and `BootloaderPerformanceRequest`.
///
/// # Safety
///
/// The implementing type must be laid out as `limine.h`'s response for the
/// request with `ID` (checked below for the two there are), with integer
/// fields only, so any bytes Limine wrote are a valid value.
unsafe trait Response: Sync + 'static {
    const ID: [u64; 2];
}

/// A request in the protocol's common layout (`limine.h`): Limine's common
/// magic and `R`'s two IDs, the revision we ask for (0), and the response
/// pointer Limine fills in.
#[repr(C)]
struct RawRequest<R: Response> {
    id: [u64; 4],
    revision: u64,
    response: UnsafeCell<*const R>,
}

// SAFETY: Limine writes `response` before the kernel starts; the kernel
// only reads it, and the response it points to is `Sync` (`Response`).
unsafe impl<R: Response> Sync for RawRequest<R> {}

impl<R: Response> RawRequest<R> {
    const fn new() -> Self {
        Self {
            id: [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b, R::ID[0], R::ID[1]],
            revision: 0,
            response: UnsafeCell::new(core::ptr::null()),
        }
    }

    fn get_response(&self) -> Option<&'static R> {
        // SAFETY: a volatile read, as Limine wrote the pointer behind the
        // compiler's back (the `limine` crate reads its own the same way).
        let ptr = unsafe { core::ptr::read_volatile(self.response.get()) };
        // SAFETY: a non-null pointer is Limine's response: an `R` (its
        // layout, from `limine.h`, integers only, per `Response`'s
        // contract), aligned, in bootloader-reclaimable memory, which
        // `mm::frame` never hands out (it allocates only from `usable`
        // entries) and the HHDM keeps mapped; never written after the
        // handoff.
        unsafe { ptr.as_ref() }
    }
}

/// `struct limine_tsc_frequency_response`.
#[repr(C)]
struct TscFrequencyResponse {
    _revision: u64,
    frequency: u64,
}

// SAFETY: laid out as `limine.h`'s response for this ID (asserted below).
unsafe impl Response for TscFrequencyResponse {
    const ID: [u64; 2] = [0x10f2ee1d87d195e4, 0xf747a2b78f6ddb31];
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

// SAFETY: laid out as `limine.h`'s response for this ID (asserted below).
unsafe impl Response for PerformanceResponse {
    const ID: [u64; 2] = [0x6b50ad9bf36d13ad, 0xdc4c7e88fc759e17];
}

// The layouts `limine.h` gives (protocol 3a0526b, as Limine v12.9.4 pins).
const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(size_of::<RawRequest<TscFrequencyResponse>>() == 48);
    assert!(offset_of!(RawRequest<TscFrequencyResponse>, revision) == 32);
    assert!(offset_of!(RawRequest<TscFrequencyResponse>, response) == 40);
    assert!(size_of::<RawRequest<PerformanceResponse>>() == 48);
    assert!(size_of::<TscFrequencyResponse>() == 16);
    assert!(offset_of!(TscFrequencyResponse, frequency) == 8);
    assert!(size_of::<PerformanceResponse>() == 32);
    assert!(offset_of!(PerformanceResponse, init_usec) == 16);
    assert!(offset_of!(PerformanceResponse, exec_usec) == 24);
};

/// Limine's raw TSC-frequency response, for the ktest to check it is what
/// went into `before_kmain`'s `LimineTimes`.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by a ktest
pub fn limine_tsc_frequency() -> Option<u64> {
    TSC_FREQUENCY.get_response().map(|r| r.frequency)
}

/// Limine's times (its two responses, or which is missing), the kernel's
/// TSC calibration and the timeline's origin, for the `boot: before kmain:`
/// line and its check (`lumen_time::handoff`).
pub fn before_kmain() -> Result<BeforeKmain, Unknown> {
    let limine = LimineTimes::from_responses(
        TSC_FREQUENCY.get_response().map(|r| r.frequency),
        PERFORMANCE.get_response().map(|r| (r.init_usec, r.exec_usec)),
    );
    BeforeKmain::new(crate::time::tsc(), limine, crate::time::boot_tsc())
}
