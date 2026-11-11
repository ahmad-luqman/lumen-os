//! Global Descriptor Table and Task State Segment.
//!
//! Long mode barely uses segmentation, but we still need: a 64-bit code
//! segment, a data segment, user segments laid out the way `syscall`/`sysret`
//! expect (M2), and a TSS whose Interrupt Stack Table gives the double-fault,
//! NMI and machine-check handlers known-good stacks. Without the IST a kernel
//! stack overflow would triple-fault instead of producing a crash report.

use super::stacks;
use core::arch::asm;
use core::mem::size_of;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
/// User selectors include RPL 3. Order (data before code) is what `sysret` needs.
#[allow(dead_code)] // used by M2 (user mode)
pub const USER_DATA: u16 = 0x18 | 3;
#[allow(dead_code)] // used by M2 (user mode)
pub const USER_CODE: u16 = 0x20 | 3;
pub const TSS: u16 = 0x28;

/// IST slots (1-based, as encoded in IDT entries).
pub const IST_DOUBLE_FAULT: u8 = 1;
pub const IST_NMI: u8 = 2;
pub const IST_MACHINE_CHECK: u8 = 3;

#[repr(C, packed(4))]
struct TaskStateSegment {
    _reserved0: u32,
    rsp: [u64; 3],
    _reserved1: u64,
    ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    iomap_base: u16,
}

#[repr(C, packed(2))]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

const GDT_ENTRIES: usize = 7; // null, kcode, kdata, udata, ucode, tss(2)

static mut TSS_SEGMENT: TaskStateSegment = TaskStateSegment {
    _reserved0: 0,
    rsp: [0; 3],
    _reserved1: 0,
    ist: [0; 7],
    _reserved2: 0,
    _reserved3: 0,
    iomap_base: size_of::<TaskStateSegment>() as u16, // no I/O bitmap
};

static mut GDT: [u64; GDT_ENTRIES] = [
    0,
    0x00af_9a00_0000_ffff, // kernel code: present, ring 0, executable, long mode
    0x00cf_9200_0000_ffff, // kernel data: present, ring 0, writable
    0x00cf_f200_0000_ffff, // user data:   present, ring 3, writable
    0x00af_fa00_0000_ffff, // user code:   present, ring 3, executable, long mode
    0,                     // TSS low  (filled in at runtime)
    0,                     // TSS high
];

/// Load the GDT and TSS and reload every segment register.
///
/// Called once, very early, on the boot CPU.
pub fn init() {
    let tss = &raw mut TSS_SEGMENT;
    // SAFETY: single-threaded early boot; nothing else touches the TSS or GDT
    // yet, and the IST stacks are static and live forever.
    unsafe {
        (*tss).ist[usize::from(IST_DOUBLE_FAULT - 1)] = stacks::DOUBLE_FAULT.top();
        (*tss).ist[usize::from(IST_NMI - 1)] = stacks::NMI.top();
        (*tss).ist[usize::from(IST_MACHINE_CHECK - 1)] = stacks::MACHINE_CHECK.top();

        let base = tss as u64;
        let limit = (size_of::<TaskStateSegment>() - 1) as u64;
        let low = (limit & 0xffff)
            | ((base & 0xff_ffff) << 16)
            | (0x89 << 40) // present, 64-bit available TSS
            | (((limit >> 16) & 0xf) << 48)
            | (((base >> 24) & 0xff) << 56);
        let high = base >> 32;
        let gdt = &raw mut GDT;
        (*gdt)[5] = low;
        (*gdt)[6] = high;

        let ptr = DescriptorTablePointer { limit: (size_of::<[u64; GDT_ENTRIES]>() - 1) as u16, base: gdt as u64 };
        asm!(
            "lgdt [{ptr}]",
            // Reload CS with a far return: push selector, push target, retfq.
            "push {code}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:x}",
            "mov gs, {tmp:x}",
            "ltr {tss:x}",
            ptr = in(reg) &raw const ptr,
            code = const KERNEL_CODE as u64,
            data = in(reg) u64::from(KERNEL_DATA),
            tss = in(reg) u64::from(TSS),
            tmp = out(reg) _,
        );
    }
}

#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
/// The CS selector currently loaded (used by tests).
pub fn current_cs() -> u16 {
    let cs: u16;
    // SAFETY: reads a segment register.
    unsafe { asm!("mov {:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags)) }
    cs
}
