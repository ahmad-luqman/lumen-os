//! x86 I/O port access.

use core::arch::asm;

/// # Safety
/// Writing to an I/O port can have arbitrary hardware side effects.
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: upheld by the caller.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) }
}

/// # Safety
/// Reading an I/O port can have hardware side effects.
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: upheld by the caller.
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags)) }
    value
}

/// # Safety
/// Writing to an I/O port can have arbitrary hardware side effects.
pub unsafe fn outl(port: u16, value: u32) {
    // SAFETY: upheld by the caller.
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags)) }
}

/// # Safety
/// Writing to an I/O port can have arbitrary hardware side effects.
pub unsafe fn outw(port: u16, value: u16) {
    // SAFETY: upheld by the caller.
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags)) }
}

/// # Safety
/// Reading an I/O port can have hardware side effects.
pub unsafe fn inw(port: u16) -> u16 {
    let value: u16;
    // SAFETY: upheld by the caller.
    unsafe { asm!("in ax, dx", out("ax") value, in("dx") port, options(nomem, nostack, preserves_flags)) }
    value
}
