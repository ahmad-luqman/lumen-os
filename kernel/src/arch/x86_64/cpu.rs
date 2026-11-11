//! Small CPU helpers.

use core::arch::asm;

/// Halt this CPU forever with interrupts disabled.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: cli/hlt only stop this CPU.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

macro_rules! read_cr {
    ($name:ident, $reg:literal) => {
        pub fn $name() -> u64 {
            let v: u64;
            // SAFETY: reading a control register has no side effects in ring 0.
            unsafe { asm!(concat!("mov {}, ", $reg), out(reg) v, options(nomem, nostack, preserves_flags)) }
            v
        }
    };
}
read_cr!(read_cr0, "cr0");
read_cr!(read_cr2, "cr2");
read_cr!(read_cr3, "cr3");
read_cr!(read_cr4, "cr4");

/// Current frame pointer (valid because we build with frame pointers forced).
#[inline(always)]
pub fn read_rbp() -> u64 {
    let v: u64;
    // SAFETY: reads a register.
    unsafe { asm!("mov {}, rbp", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}

#[allow(dead_code)] // HAL surface
#[inline(always)]
pub fn read_rsp() -> u64 {
    let v: u64;
    // SAFETY: reads a register.
    unsafe { asm!("mov {}, rsp", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}

/// Time-stamp counter. Uncalibrated in M0 (calibration is an M1 timer task).
#[allow(dead_code)] // HAL surface; boot timeline in M1
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: rdtsc is unprivileged and side-effect free.
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) }
    (u64::from(hi) << 32) | u64::from(lo)
}
