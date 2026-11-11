//! QEMU `isa-debug-exit` device: lets tests end QEMU with a status code.
//!
//! QEMU exits with `(code << 1) | 1`, so it can never report 0. The harness
//! treats exactly `Success` as a pass and everything else (including a clean
//! QEMU quit or a triple-fault reset) as a failure.
//!
//! Only used when the kernel command line contains `lumen.qemu_exit`; on real
//! hardware nothing is written to the port.

use super::port::outl;

const PORT: u16 = 0xf4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ExitCode {
    /// All tests passed. QEMU exit status 33.
    Success = 0x10,
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    /// A test failed. QEMU exit status 35.
    Failure = 0x11,
    /// The crash reporter ran. QEMU exit status 37.
    Crash = 0x12,
}

/// Exit QEMU. Returns only if the device is absent.
pub fn exit(code: ExitCode) {
    // SAFETY: port 0xf4 is the isa-debug-exit device in our QEMU config;
    // callers only reach this when `lumen.qemu_exit` was requested.
    unsafe { outl(PORT, code as u32) }
}
