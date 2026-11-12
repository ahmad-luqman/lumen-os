//! Interrupt controllers: the legacy 8259 PICs remapped and masked, this
//! CPU's local APIC enabled, every I/O APIC input masked (drivers route the
//! ones they use). Vectors and dispatch are in `arch::idt`.

use alloc::vec::Vec;

use log::{info, warn};
use lumen_mm::PhysAddr;

use crate::arch::{apic, pic};
use crate::mm::paging;
use crate::sync::SpinLock;

/// The I/O APICs, with the first global system interrupt each handles.
static IO_APICS: SpinLock<Vec<(apic::IoApic, u32)>> = SpinLock::new(Vec::new());

/// Set up the interrupt controllers. Interrupts stay off; needs `map_mmio`
/// and the heap, and ACPI's description if there is one. Panics if the
/// local APIC can't be used: the kernel can't keep time without it.
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
        let ioapic = unsafe { apic::IoApic::new(virt) };
        ioapic.mask_all();
        inputs += ioapic.entries();
        io_apics.push((ioapic, io.gsi_base));
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
        let io_apics = IO_APICS.lock();
        kassert!(!io_apics.is_empty());
        for (io, _) in io_apics.iter() {
            for i in 0..io.entries() {
                kassert!(io.entry(i) & (1 << 16) != 0);
            }
        }
        kassert!(apic::lapic_ready());
        kassert_eq!(apic::lapic_id(), crate::arch::apic_id());
        Ok(())
    }
);

crate::ktest!(
    fn ioapic_entries_read_back_as_written() {
        // Program an input (masked, so nothing fires), read it back, restore.
        use crate::kassert_eq;
        let io_apics = IO_APICS.lock();
        let (io, _) = io_apics.first().ok_or("no I/O APIC")?;
        let old = io.entry(1);
        let new = apic::redirection(crate::arch::idt::IOAPIC_BASE + 1, 0, 0b1111, true);
        io.set_entry(1, new);
        let read = io.entry(1);
        io.set_entry(1, old);
        // Vector, active low (bit 13), level (bit 15), masked (bit 16).
        kassert_eq!(read & 0x1_a0ff, u64::from(crate::arch::idt::IOAPIC_BASE + 1) | 1 << 13 | 1 << 15 | 1 << 16);
        kassert_eq!(io.entry(1), old);
        Ok(())
    }
);

crate::ktest!(
    fn device_vectors_are_not_fatal() {
        // A vector nothing handles is counted (and warned about once), the
        // spurious vector only counted; both return.
        use core::sync::atomic::Ordering::Relaxed;

        use crate::arch::idt::{SPURIOUS_IRQS, UNEXPECTED_IRQS};
        use crate::kassert_eq;
        let (unexpected, spurious) = (UNEXPECTED_IRQS.load(Relaxed), SPURIOUS_IRQS.load(Relaxed));
        // SAFETY: software interrupts on vectors the dispatcher handles and
        // returns from; no register is clobbered (the entry stub saves all).
        unsafe {
            core::arch::asm!("int 0x60", "int 0x60", "int 0xff");
        }
        kassert_eq!((UNEXPECTED_IRQS.load(Relaxed), SPURIOUS_IRQS.load(Relaxed)), (unexpected + 2, spurious + 1));
        Ok(())
    }
);

crate::ktest!(
    fn mask_all_masks_an_unmasked_input() {
        // Firmware (and QEMU at reset) leaves inputs masked, so unmask one
        // first, with interrupts off so nothing it raises is taken.
        use crate::kassert_eq;
        let io_apics = IO_APICS.lock();
        let (io, _) = io_apics.first().ok_or("no I/O APIC")?;
        let old = io.entry(1);
        io.set_entry(1, apic::redirection(crate::arch::idt::IOAPIC_BASE + 1, 0, 0, false));
        let unmasked = io.entry(1) & (1 << 16);
        io.mask_all();
        let masked = io.entry(1) & (1 << 16);
        io.set_entry(1, old);
        kassert_eq!((unmasked, masked), (0, 1 << 16));
        Ok(())
    }
);
