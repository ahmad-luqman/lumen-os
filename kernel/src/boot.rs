//! Limine boot protocol requests.
//!
//! Limine scans the kernel image for these structures (between the start and
//! end markers) and fills in their responses before jumping to `_start`.
//! Everything the kernel knows about the machine at boot comes from here.

use limine::BaseRevision;
use limine::memory_map::EntryType;
use limine::request::{
    ExecutableAddressRequest, ExecutableCmdlineRequest, ExecutableFileRequest, FramebufferRequest, HhdmRequest,
    MemoryMapRequest, RequestsEndMarker, RequestsStartMarker, RsdpRequest, StackSizeRequest,
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
