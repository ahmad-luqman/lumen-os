//! Interrupt controllers: the legacy 8259 PICs remapped and masked, this
//! CPU's local APIC enabled, every I/O APIC input masked; drivers route the
//! ones they use ([`route_isa_irq`]). Vectors and dispatch are in
//! `arch::idt`.

use alloc::vec::Vec;
use core::fmt;

use log::{info, warn};
use lumen_mm::PhysAddr;

use crate::arch::apic::RedirectionEntry;
use crate::arch::{apic, idt, pic};
use crate::mm::paging;
use crate::sync::SpinLock;

/// An I/O APIC, the first global system interrupt it handles, and which of
/// its inputs are routed (a bit each: `apic::MAX_INPUTS` fit).
struct IoApic {
    regs: apic::IoApic,
    gsi_base: u32,
    routed: u128,
}

const _: () = assert!(apic::MAX_INPUTS <= u128::BITS);

impl IoApic {
    /// Its input for global system interrupt `gsi`, if it has one.
    fn input(&self, gsi: u32) -> Option<u32> {
        gsi.checked_sub(self.gsi_base).filter(|&i| i < self.regs.entries())
    }

    fn is_routed(&self, input: u32) -> bool {
        self.routed & (1 << input) != 0
    }
}

/// The I/O APIC with an input for `gsi`, and that input.
fn find_input(io_apics: &mut [IoApic], gsi: u32) -> Option<(&mut IoApic, u32)> {
    io_apics.iter_mut().find_map(|io| io.input(gsi).map(|input| (io, input)))
}

/// ISA interrupt `irq`'s GSI and MADT INTI flags: its interrupt override's,
/// else `irq` itself and 0 (the bus's default: edge, active high).
fn isa_gsi(irq: u8) -> (u32, u16) {
    let info = crate::acpi::info();
    let ov = info.and_then(|i| i.overrides.iter().find(|o| o.bus == 0 && o.irq == irq));
    ov.map_or((u32::from(irq), 0), |o| (o.gsi, o.flags))
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
        io_apics.push(IoApic { regs, gsi_base: io.gsi_base, routed: 0 });
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

/// An ISA interrupt routed to the boot CPU: only [`route_isa_irq`] makes
/// one, so [`set_masked`] can only unmask an input with a handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    irq: u8,
    gsi: u32,
    vector: u8,
}

impl Route {
    pub fn irq(self) -> u8 {
        self.irq
    }

    /// Its global system interrupt (the MADT may override `irq`'s).
    pub fn gsi(self) -> u32 {
        self.gsi
    }

    pub fn vector(self) -> u8 {
        self.vector
    }
}

/// Why [`route_isa_irq`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteError {
    /// No I/O APIC has an input for the GSI (none at all without ACPI).
    NoInput(u32),
    /// The GSI is past the I/O APIC vectors (`IOAPIC_BASE + gsi` > 0xef).
    NoVector(u32),
    /// The input (or its vector) is routed already.
    Taken(u32),
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RouteError::NoInput(gsi) => write!(f, "no I/O APIC input for GSI {gsi}"),
            RouteError::NoVector(gsi) => write!(f, "GSI {gsi} has no vector"),
            RouteError::Taken(gsi) => write!(f, "GSI {gsi} is routed already"),
        }
    }
}

/// Route ISA interrupt `irq` to the boot CPU on vector `IOAPIC_BASE + gsi`,
/// with `handler` registered for it, and leave it **masked**: the driver
/// unmasks it ([`set_masked`]) once its device is ready. The MADT's
/// interrupt override for `irq`, if any, gives the GSI, polarity and
/// trigger; without one the GSI is `irq`, active high and edge triggered
/// (ISA). Only on the boot CPU (asserted): its APIC id is the destination,
/// and drivers rely on every I/O APIC interrupt arriving there.
pub fn route_isa_irq(irq: u8, handler: fn()) -> Result<Route, RouteError> {
    assert_eq!(crate::arch::cpu_index(), 0, "I/O APIC interrupts are routed from (and to) the boot CPU");
    let (gsi, flags) = isa_gsi(irq);
    let vector = u8::try_from(gsi)
        .ok()
        .and_then(|g| idt::IOAPIC_BASE.checked_add(g))
        .filter(|&v| v <= idt::IOAPIC_LAST)
        .ok_or(RouteError::NoVector(gsi))?;
    let mut io_apics = IO_APICS.lock();
    let (io, input) = find_input(&mut io_apics, gsi).ok_or(RouteError::NoInput(gsi))?;
    if io.is_routed(input) {
        return Err(RouteError::Taken(gsi));
    }
    match idt::set_irq_handler(vector, handler) {
        Ok(()) => {}
        Err(idt::HandlerError::Taken) => return Err(RouteError::Taken(gsi)),
        Err(idt::HandlerError::NotAnIoApicVector) => unreachable!("{vector:#x} was checked to be an I/O APIC vector"),
    }
    let dest = u8::try_from(apic::lapic_id()).expect("the boot CPU's APIC id fits an I/O APIC destination");
    io.regs.set_entry(input, RedirectionEntry::isa(vector, dest, flags, true));
    io.routed |= 1 << input;
    Ok(Route { irq, gsi, vector })
}

/// Mask or unmask a routed input.
pub fn set_masked(route: Route, masked: bool) {
    let mut io_apics = IO_APICS.lock();
    let (io, input) = find_input(&mut io_apics, route.gsi).expect("a route has an input");
    assert!(io.is_routed(input), "GSI {} isn't routed", route.gsi);
    io.regs.update_entry(input, |e| e.set_masked(masked));
}

/// Whether ISA interrupt `irq`'s input (after the MADT's override) is
/// routed and unmasked.
#[cfg(feature = "ktest")]
pub fn is_unmasked_isa(irq: u8) -> bool {
    let (gsi, _) = isa_gsi(irq);
    let mut io_apics = IO_APICS.lock();
    find_input(&mut io_apics, gsi).is_some_and(|(io, i)| io.is_routed(i) && !io.regs.entry(i).masked())
}

/// Map a page of controller registers (uncached), or panic naming it.
fn map(phys: u64, what: &str) -> u64 {
    let addr = PhysAddr::new(phys).unwrap_or_else(|| panic!("{what} at {phys:#x}: not a physical address"));
    paging::map_mmio(addr, 4096).unwrap_or_else(|e| panic!("mapping the {what} at {phys:#x}: {e}")).as_u64()
}

/// An input of `io` that nothing routes, for tests to program.
#[cfg(feature = "ktest")]
fn scratch_input(io: &IoApic) -> Result<u32, &'static str> {
    (0..io.regs.entries()).rev().find(|&i| !io.is_routed(i)).ok_or("no unrouted input")
}

crate::ktest!(
    fn controllers_are_masked() {
        // Every I/O APIC input but the routed ones (the keyboard's, and any
        // a ktest routed).
        use crate::{kassert, kassert_eq};
        kassert_eq!(pic::masks(), 0xffff);
        let mut io_apics = IO_APICS.lock();
        kassert!(!io_apics.is_empty());
        for io in io_apics.iter_mut() {
            for i in 0..io.regs.entries() {
                kassert!(io.is_routed(i) || io.regs.entry(i).masked());
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
        let e = |flags| RedirectionEntry::isa(0x51, 0, flags, false).bits();
        kassert_eq!(e(0b0000), 0x51);
        kassert_eq!(e(0b0001), 0x51);
        kassert_eq!(e(0b0011), 0x51 | LOW);
        kassert_eq!(e(0b0100), 0x51);
        kassert_eq!(e(0b1100), 0x51 | LEVEL);
        kassert_eq!(e(0b1111), 0x51 | LOW | LEVEL);
        kassert_eq!(e(0b1101), 0x51 | LEVEL);
        kassert_eq!(RedirectionEntry::isa(0x60, 3, 0, true).bits(), 0x60 | 1 << 16 | 3 << 56);
        Ok(())
    }
);

crate::ktest!(
    fn redirection_entry_roundtrip() {
        // Every field set to a non-default value encodes where the 82093AA
        // datasheet puts it, survives a write to an unrouted input and a
        // read back (masked, so nothing fires), and the read-only bits read
        // back clear; `update_entry` changes one field and keeps the rest.
        use crate::arch::apic::{Delivery, Polarity, Trigger};
        use crate::{kassert, kassert_eq};
        let mut e = RedirectionEntry::MASKED_DEFAULT;
        e.set_vector(0xa5);
        e.set_delivery(Delivery::LowestPriority);
        e.set_logical(true);
        e.set_polarity(Polarity::ActiveLow);
        e.set_trigger(Trigger::Level);
        e.set_destination(0x0f);
        kassert_eq!(e.bits(), 0xa5 | 0b001 << 8 | 1 << 11 | 1 << 13 | 1 << 15 | 1 << 16 | 0x0f << 56);
        let mut io_apics = IO_APICS.lock();
        let io = io_apics.first_mut().ok_or("no I/O APIC")?;
        let input = scratch_input(io)?;
        let regs = &mut io.regs;
        let old = regs.entry(input);
        regs.set_entry(input, e);
        let read = regs.entry(input);
        let updated = regs.update_entry(input, |r| r.set_vector(0x5a));
        let reread = regs.entry(input);
        regs.set_entry(input, old);
        let restored = regs.entry(input);
        drop(io_apics);
        kassert_eq!(read.vector(), 0xa5);
        kassert_eq!(read.delivery(), Some(Delivery::LowestPriority));
        kassert!(read.logical());
        kassert_eq!((read.polarity(), read.trigger()), (Polarity::ActiveLow, Trigger::Level));
        kassert!(read.masked());
        kassert_eq!(read.destination(), 0x0f);
        kassert!(!read.remote_irr() && !read.delivery_pending());
        kassert_eq!((updated.vector(), reread.vector()), (0x5a, 0x5a));
        kassert_eq!(reread.destination(), 0x0f);
        kassert_eq!(restored, old);
        Ok(())
    }
);

crate::ktest!(
    fn mask_all_masks_an_unmasked_input() {
        // Firmware (and QEMU at reset) leaves inputs masked, so unmask an
        // unrouted one first. The held lock keeps interrupts off, and every
        // entry (the routed ones too) is restored before it drops (an edge
        // raised meanwhile would be latched and taken afterwards, as an
        // unexpected interrupt).
        use crate::kassert_eq;
        let mut io_apics = IO_APICS.lock();
        let io = io_apics.first_mut().ok_or("no I/O APIC")?;
        let input = scratch_input(io)?;
        let regs = &mut io.regs;
        let saved: Vec<RedirectionEntry> = (0..regs.entries()).map(|i| regs.entry(i)).collect();
        regs.set_entry(input, RedirectionEntry::isa(idt::IOAPIC_BASE + input as u8, 0, 0, false));
        let unmasked = regs.entry(input).masked();
        regs.mask_all();
        let all_masked = (0..regs.entries()).all(|i| regs.entry(i).masked());
        for (i, e) in saved.into_iter().enumerate() {
            regs.set_entry(i as u32, e);
        }
        drop(io_apics);
        kassert_eq!((unmasked, all_masked), (false, true));
        Ok(())
    }
);

crate::ktest!(
    fn routing_takes_the_madt_override_and_refuses_twice() {
        // ISA IRQ 0 (the PIT) is overridden to GSI 2 on QEMU's q35: routed
        // there, masked, on vector IOAPIC_BASE + 2 to this CPU; routing it
        // again, or the keyboard's IRQ 1, is refused. (It stays routed and
        // masked: nothing unmasks it.)
        use crate::{kassert, kassert_eq};
        fn nothing() {}
        let route = route_isa_irq(0, nothing).map_err(|e| {
            crate::ktest::diag(format_args!("{e}"));
            "routing IRQ 0 failed"
        })?;
        kassert_eq!(route, Route { irq: 0, gsi: 2, vector: idt::IOAPIC_BASE + 2 });
        let entry = {
            let mut io_apics = IO_APICS.lock();
            let (io, input) = find_input(&mut io_apics, 2).ok_or("no input for GSI 2")?;
            io.regs.entry(input)
        };
        kassert!(entry.masked());
        kassert_eq!((entry.vector(), u32::from(entry.destination())), (route.vector, apic::lapic_id()));
        kassert_eq!(route_isa_irq(0, nothing), Err(RouteError::Taken(2)));
        kassert_eq!(route_isa_irq(1, nothing), Err(RouteError::Taken(1)));
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
                pic7 = const idt::PIC_SPURIOUS_MASTER,
                pic15 = const idt::PIC_SPURIOUS_SLAVE,
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
