//! Interrupt controllers: the legacy 8259 PICs remapped and masked, this
//! CPU's local APIC enabled, every I/O APIC input masked (drivers route the
//! ones they use). Vectors and dispatch are in `arch::idt`.

use alloc::vec::Vec;

use log::{info, warn};
use lumen_mm::PhysAddr;

use crate::arch::{apic, pic};
use crate::mm::paging;
use crate::sync::SpinLock;

/// An I/O APIC and the first global system interrupt it handles.
struct IoApic {
    // Read by ktests; routing (device drivers, M3) reads both.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    regs: apic::IoApic,
    #[allow(dead_code)]
    gsi_base: u32,
}

static IO_APICS: SpinLock<Vec<IoApic>> = SpinLock::new(Vec::new());

/// Set up the interrupt controllers. Interrupts stay off; needs `map_mmio`
/// and the heap, and ACPI's description if there is one. Panics if the
/// local APIC can't be used (the kernel can't keep time without it), or an
/// I/O APIC ACPI lists can't be mapped (its address is then wrong, and
/// leaving its inputs as firmware set them would be worse).
pub fn init() {
    pic::remap_and_mask();
    let base = apic::apic_base();
    assert!(base.enabled, "the local APIC is disabled in IA32_APIC_BASE");
    assert!(!base.x2apic, "the local APIC is in x2APIC mode; only xAPIC (memory-mapped) is supported");
    let info = crate::acpi::info();
    if let Some(madt) = info.map(|i| i.local_apic).filter(|&a| a != 0 && a != base.phys) {
        warn!(target: "irq", "the MADT puts the local APIC at {madt:#x}, IA32_APIC_BASE at {:#x}: using the latter", base.phys);
    }
    let virt = map(base.phys, "local APIC");
    // SAFETY: `virt` maps this CPU's local APIC registers for good, and it is
    // in xAPIC mode (asserted above).
    unsafe { apic::set_lapic(virt) };

    let mut io_apics = IO_APICS.lock();
    let mut inputs = 0;
    for io in info.map_or(&[][..], |i| &i.io_apics) {
        let virt = map(u64::from(io.address), "I/O APIC");
        // SAFETY: `virt` maps this I/O APIC's registers for good, and only
        // this module drives it.
        let mut regs = unsafe { apic::IoApic::new(virt) };
        regs.mask_all();
        inputs += regs.entries();
        io_apics.push(IoApic { regs, gsi_base: io.gsi_base });
    }
    if io_apics.is_empty() {
        warn!(target: "irq", "no I/O APIC known: device interrupts can't be routed");
    }
    info!(
        target: "irq",
        "local APIC {} at {:#x}; 8259 PICs masked; {} I/O APIC inputs masked",
        apic::lapic_id(),
        base.phys,
        inputs
    );
}

/// Map a page of controller registers (uncached), or panic naming it.
fn map(phys: u64, what: &str) -> u64 {
    let addr = PhysAddr::new(phys).unwrap_or_else(|| panic!("{what} at {phys:#x}: not a physical address"));
    paging::map_mmio(addr, 4096).unwrap_or_else(|e| panic!("mapping the {what} at {phys:#x}: {e}")).as_u64()
}

crate::ktest!(
    fn controllers_are_masked() {
        use crate::{kassert, kassert_eq};
        kassert_eq!(pic::masks(), 0xffff);
        let mut io_apics = IO_APICS.lock();
        kassert!(!io_apics.is_empty());
        for io in io_apics.iter_mut() {
            for i in 0..io.regs.entries() {
                kassert!(io.regs.entry(i) & apic::REDIRECTION_MASKED != 0);
            }
        }
        kassert!(apic::lapic_ready());
        kassert_eq!(apic::lapic_id(), crate::arch::apic_id());
        Ok(())
    }
);

crate::ktest!(
    fn redirection_entries_encode_the_madt_flags() {
        // MADT INTI flags: bits 0-1 polarity (0b11 active low), bits 2-3
        // trigger (0b11 level); 0b00 and 0b01 are the bus's default and
        // active high / edge.
        use crate::kassert_eq;
        const LOW: u64 = 1 << 13;
        const LEVEL: u64 = 1 << 15;
        let e = |flags| apic::redirection(0x51, 0, flags, false);
        kassert_eq!(e(0b0000), 0x51);
        kassert_eq!(e(0b0001), 0x51);
        kassert_eq!(e(0b0011), 0x51 | LOW);
        kassert_eq!(e(0b0100), 0x51);
        kassert_eq!(e(0b1100), 0x51 | LEVEL);
        kassert_eq!(e(0b1111), 0x51 | LOW | LEVEL);
        kassert_eq!(e(0b1101), 0x51 | LEVEL);
        kassert_eq!(apic::redirection(0x60, 3, 0, true), 0x60 | apic::REDIRECTION_MASKED | 3 << 56);
        Ok(())
    }
);

crate::ktest!(
    fn ioapic_entries_read_back_as_written() {
        // Program an input (masked, so nothing fires), read it back, restore.
        use crate::kassert_eq;
        let mut io_apics = IO_APICS.lock();
        let io = &mut io_apics.first_mut().ok_or("no I/O APIC")?.regs;
        let old = io.entry(1);
        let new = apic::redirection(crate::arch::idt::IOAPIC_BASE + 1, 0, 0b1111, true);
        io.set_entry(1, new);
        let read = io.entry(1);
        io.set_entry(1, old);
        kassert_eq!(read, new);
        kassert_eq!(io.entry(1), old);
        Ok(())
    }
);

crate::ktest!(
    fn mask_all_masks_an_unmasked_input() {
        // Firmware (and QEMU at reset) leaves inputs masked, so unmask one
        // first. The held lock keeps interrupts off, and the input is masked
        // again before it drops (an edge raised meanwhile would be latched
        // and taken afterwards, as an unexpected interrupt).
        use crate::kassert_eq;
        let mut io_apics = IO_APICS.lock();
        let io = &mut io_apics.first_mut().ok_or("no I/O APIC")?.regs;
        let old = io.entry(1);
        io.set_entry(1, apic::redirection(crate::arch::idt::IOAPIC_BASE + 1, 0, 0, false));
        let unmasked = io.entry(1) & apic::REDIRECTION_MASKED;
        io.mask_all();
        let masked = io.entry(1) & apic::REDIRECTION_MASKED;
        io.set_entry(1, old);
        kassert_eq!((unmasked, masked), (0, apic::REDIRECTION_MASKED));
        Ok(())
    }
);

crate::ktest!(
    fn device_vectors_are_not_fatal() {
        // A vector nothing handles, or an 8259 line other than IRQ 7 and 15,
        // is counted as unexpected (and warned about once); the spurious
        // vectors (the local APIC's, the 8259s' IRQ 7 and 15) only counted;
        // a local APIC error counted and logged. All return.
        use crate::arch::idt;
        use crate::kassert_eq;
        let before = idt::strays();
        // SAFETY: software interrupts on vectors the dispatcher handles and
        // returns from; no register is clobbered (the entry stub saves all).
        unsafe {
            core::arch::asm!(
                "int {unused}", "int {unused}", "int {spurious}", "int {pic7}", "int {pic15}", "int {error}", "int {pic1}",
                unused = const idt::UNUSED,
                spurious = const idt::SPURIOUS,
                pic7 = const idt::PIC_BASE + 7,
                pic15 = const idt::PIC_BASE + 15,
                error = const idt::LAPIC_ERROR,
                pic1 = const idt::PIC_BASE + 1,
            );
        }
        let after = idt::strays();
        kassert_eq!(after.unexpected, before.unexpected + 3);
        kassert_eq!(after.spurious, before.spurious + 3);
        kassert_eq!(after.lapic_errors, before.lapic_errors + 1);
        Ok(())
    }
);
