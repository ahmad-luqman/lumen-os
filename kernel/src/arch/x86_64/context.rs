//! Switching between kernel threads: the callee-saved registers and `rsp`.
//!
//! [`switch`] is an ordinary `extern "C"` call to its caller, so the
//! compiler already keeps every caller-saved register it needs across it;
//! only the callee-saved ones (rbx, rbp, r12-r15, System V ABI) and `rsp`
//! belong to the thread. There is no SSE or x87 state to save: the kernel
//! has none (`make audit` proves it uses no SSE/AVX). RFLAGS isn't saved
//! either: the scheduler switches only with interrupts off, and the
//! direction flag is clear at every call (ABI).
//!
//! A new thread's stack is laid out ([`prepare`]) as if it had called
//! [`switch`] from [`thread_start`], so the first switch to it "returns"
//! there, with `rbp` = 0: the frame-pointer chain of everything it runs ends
//! at `thread_start`, as a CPU's ends at `_start`.

/// Callee-saved registers [`switch`] pushes (rbp, rbx, r12, r13, r14, r15).
const SAVED_REGISTERS: u64 = 6;

/// Save this thread's callee-saved registers and `rsp` (into `*save`), then
/// load another's from `load` (a value an earlier `switch` saved, or one
/// [`prepare`] made) and return into it. Returns, to the caller, when a later
/// `switch` loads what this one saved.
///
/// # Safety
/// Interrupts are off (nothing may run on either stack half-switched);
/// `save` is where the running thread's `rsp` is kept until it runs again;
/// `load` was saved by `switch` (or made by `prepare`) on a stack that is
/// still mapped and that no CPU is running on.
#[unsafe(naked)]
pub unsafe extern "C" fn switch(save: *mut u64, load: u64) {
    core::arch::naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}

/// Where every new thread starts (the first [`switch`] to it returns here),
/// on its own empty stack with `rsp` 16-byte aligned and `rbp` = 0: it calls
/// the thread's entry point, kept in r12 by [`prepare`], which never returns.
/// The call leaves this function's address as the outermost frame's return
/// address, so a backtrace ends here.
#[unsafe(naked)]
unsafe extern "C" fn thread_start() -> ! {
    core::arch::naked_asm!("call r12", "ud2")
}

/// Lay out a new thread's stack, whose top is `top`, so that the first
/// [`switch`] to the returned `rsp` starts `entry` (through [`thread_start`])
/// with `rbp` = 0.
///
/// # Safety
/// `top` is the 16-byte-aligned top of a mapped, writable stack at least a
/// page long that nothing runs on, and that stays mapped until the thread
/// has finished.
pub unsafe fn prepare(top: u64, entry: extern "C" fn() -> !) -> u64 {
    assert!(top.is_multiple_of(16), "a thread's stack top must be 16-byte aligned");
    // From the top down: the return address `switch`'s `ret` pops, then
    // what its pops load: rbp (0, the end of the frame chain), rbx, r12 (the
    // entry point, for `thread_start`), r13, r14, r15. After the `ret`, rsp
    // is `top`, so `thread_start`'s call leaves the entry with rsp 8 below a
    // 16-byte boundary, as a function expects on entry.
    let frame: [u64; SAVED_REGISTERS as usize + 1] = [
        0,                                // r15 (lowest address: the saved rsp)
        0,                                // r14
        0,                                // r13
        entry as usize as u64,            // r12
        0,                                // rbx
        0,                                // rbp
        thread_start as *const () as u64, // return address
    ];
    let rsp = top - 8 * (SAVED_REGISTERS + 1);
    // SAFETY: the caller vouches that the stack is mapped, writable, at least
    // a page (more than these 56 bytes) and unused.
    unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), rsp as *mut u64, frame.len()) };
    rsp
}
