//! The Lumen kernel.
//!
//! Boot order (see docs/ARCHITECTURE.md, "Boot"):
//! 1. `_start` (naked): clear rbp so backtraces end here, align the stack.
//!    `kmain` then reads the TSC (a bare `rdtsc`, which can't fault): the
//!    boot timeline's and the log's origin.
//! 2. `arch::init_early`: serial, GDT/TSS, IDT. From here on a CPU exception
//!    produces a crash report, except a #DF from a broken stack pointer
//!    before step 4 (no IST stacks yet), which is a triple fault.
//! 3. Logger, Limine responses, symbol table, memory map, frame allocator,
//!    the kernel's own page tables and the heap (`mm::init`).
//! 4. Allocated kernel stacks with guard pages: the IST stacks (#DF, NMI and
//!    #MC ran on the boot stack until now) are installed, then `kmain`
//!    switches from Limine's boot stack to the main kernel stack and
//!    continues in `kmain_on_kernel_stack`.
//! 5. ACPI tables (CPUs, interrupt controllers, HPET, the MCFG), the PCI
//!    functions in ECAM configuration space (`pci::devices`), the interrupt
//!    controllers (8259s masked, local APIC on, I/O APIC inputs masked), the
//!    TSC and local APIC timer calibrated, interrupts on, the scheduler
//!    started on every CPU (`kmain`'s context becomes the `main` thread);
//!    then the framebuffer and its terminal, the PS/2 keyboard, the drivers
//!    (`drivers::probe_all`: each PCI function offered to the driver
//!    table), `boot complete` and the boot timeline, then either the
//!    in-kernel tests (`--features ktest`) or the shells (`shell::run`).

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod app;
mod arch;
mod boot;
mod cmdline;
mod crash;
mod demo;
mod desktop;
mod drivers;
mod fb;
mod font;
mod input;
mod interrupts;
mod keyboard;
mod klog;
mod ktest;
mod mm;
mod mouse;
mod pci;
mod perf;
mod power;
mod rng;
mod rtc;
mod sched;
mod screenshot;
mod script;
mod serial_in;
mod shell;
mod smp;
mod sound;
mod symbols;
mod sync;
mod taskbar;
mod term;
mod termwin;
mod time;
mod trace;
mod ui;
mod virtio;
mod vt;

use log::{LevelFilter, debug, info, warn};
use lumen_mm::stacks::{StackKind, StackRange};
use trace::{BootPhase, Kind};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Kernel entry point. Limine jumps here with a 16-byte aligned stack top and
/// a 0 return address pushed on it (Limine protocol, x86-64 entry state).
#[unsafe(naked)]
#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor ebp, ebp",   // end of the frame-pointer chain
        "lea rdi, [rsp + 8]", // stack top (above Limine's 0 return address)
        "and rsp, -16",
        "call {kmain}",
        "ud2",
        kmain = sym kmain,
    )
}

/// Unmangled so debuggers can `break kmain` without knowing Rust mangling.
#[unsafe(no_mangle)]
extern "C" fn kmain(stack_top: u64) -> ! {
    // The boot timeline's origin: a bare `rdtsc`, which can't fault, so GDT
    // and IDT setup still come first in effect (hard rule 6).
    let entry = arch::rdtsc();
    arch::init_early();
    time::mark_boot(entry);
    trace::record_at(trace::boot_ring(), entry, Kind::BootPhase, trace::args([BootPhase::Entry as u64]));
    ktrace!(Kind::BootPhase, BootPhase::ArchInit);
    // Only claim the size we asked for if Limine confirms it: the frame
    // allocator reserves this range and the crash reporter walks it.
    let stack_size =
        if boot::STACK_SIZE.get_response().is_some() { boot::BOOT_STACK_SIZE } else { boot::LIMINE_DEFAULT_STACK_SIZE };
    arch::stacks::set_boot_stack(stack_top, stack_size);

    if let Some(resp) = boot::CMDLINE.get_response() {
        cmdline::set(resp.cmdline().to_str().unwrap_or(""));
    }
    let level = cmdline::value("lumen.log").and_then(klog::parse_level).unwrap_or(LevelFilter::Info);
    klog::init(level);
    ktrace!(Kind::BootPhase, BootPhase::Logger);
    info!(target: "boot", "Lumen {VERSION} (x86_64) booting");
    info!(target: "boot", "cmdline: '{}'", cmdline::get());

    if !boot::BASE_REVISION.is_supported() {
        panic!("Limine does not support base revision 3 (loaded {:?})", boot::BASE_REVISION.loaded_revision());
    }
    if boot::STACK_SIZE.get_response().is_none() {
        warn!(target: "boot", "boot stack size request ignored; assuming Limine's 64 KiB default");
    }

    match boot::EXECUTABLE_FILE.get_response() {
        Some(resp) => match symbols::init(file_bytes(resp.file())) {
            Ok(n) => info!(target: "boot", "symbols: {n} entries"),
            Err(e) => warn!(target: "boot", "symbols unavailable: {e}"),
        },
        None => warn!(target: "boot", "no executable file response; crash reports unsymbolized"),
    }
    ktrace!(Kind::BootPhase, BootPhase::Symbols);

    if let Some(addr) = boot::EXECUTABLE_ADDRESS.get_response() {
        info!(target: "boot", "kernel at phys {:#x} virt {:#x}", addr.physical_base(), addr.virtual_base());
    }
    if let Some(hhdm) = boot::HHDM.get_response() {
        info!(target: "boot", "hhdm offset {:#x}", hhdm.offset());
    }
    log_memory_map();
    mm::init();
    ktrace!(Kind::BootPhase, BootPhase::Memory);
    let main_stack = install_stacks();
    ktrace!(Kind::BootPhase, BootPhase::Stacks);
    // SAFETY: the main stack is leaked (never freed), mapped and used by
    // nothing else, and its top is page-aligned; the boot stack stays mapped
    // and reserved and is never reused, so the chain back into `kmain` stays
    // walkable.
    unsafe { arch::switch_stack(main_stack.top, kmain_on_kernel_stack) }
}

/// Allocate the kernel's stacks (each with an unmapped guard page below it):
/// the IST stacks, installed at once, and the main stack, returned for
/// `kmain` to switch to. No static stacks remain (hard rule 4).
fn install_stacks() -> StackRange {
    use mm::stack::{KernelStack, MAIN_STACK_SIZE, STACK_SIZE};
    let stack = |size, kind: StackKind| {
        KernelStack::new(size, kind).unwrap_or_else(|e| panic!("allocating the {} stack: {e}", kind.name())).leak()
    };
    let df = stack(STACK_SIZE, StackKind::DoubleFault);
    let nmi = stack(STACK_SIZE, StackKind::Nmi);
    let mc = stack(STACK_SIZE, StackKind::MachineCheck);
    // SAFETY: leaked, mapped stacks nothing else uses; the boot CPU, before
    // any other starts, interrupts off.
    unsafe { arch::install_ist_stacks(df.top, nmi.top, mc.top) };
    let main = stack(MAIN_STACK_SIZE, StackKind::Main);
    info!(
        target: "mem",
        "stacks: main {:#x}..{:#x}, IST {:#x} {:#x} {:#x} (guard page below each)",
        main.bottom,
        main.top,
        df.bottom,
        nmi.bottom,
        mc.bottom
    );
    main
}

/// The rest of boot, on the allocated main stack.
extern "C" fn kmain_on_kernel_stack() -> ! {
    acpi::init();
    ktrace!(Kind::BootPhase, BootPhase::Acpi);
    // Sizing a BAR turns its device's decoding off for a moment: this runs
    // on this CPU alone (the others start below), with interrupts off,
    // before the framebuffer's terminal draws (the screen is in the VGA
    // BAR).
    pci::init();
    ktrace!(Kind::BootPhase, BootPhase::Pci);
    interrupts::init();
    // The other CPUs halt once started instead of spinning in the bootloader
    // during the clock calibration below.
    let started = smp::start_all();
    ktrace!(Kind::BootPhase, BootPhase::Interrupts);
    time::init();
    arch::irq::enable();
    info!(target: "irq", "interrupts on");
    time::sleep(10_000_000);
    // A working timer's first tick can be late under QEMU's TCG on a loaded
    // host (none in the first 10 ms, then ticking normally, #45): wait up to
    // 100 ms in all before warning that it doesn't tick.
    if time::ticks() == 0 {
        let late = time::now_ns();
        if time::wait_for_tick(0, 90_000_000) {
            let ms = (time::now_ns() - late) / 1_000_000 + 10;
            info!(target: "irq", "first timer interrupt after about {ms} ms (late, not missing)");
        } else {
            warn!(target: "irq", "no timer interrupt in 100 ms");
        }
    }
    ktrace!(Kind::BootPhase, BootPhase::Time);
    smp::wait_online(started);
    sched::init();
    ktrace!(Kind::BootPhase, BootPhase::Smp);
    match boot::FRAMEBUFFER.get_response().and_then(|r| r.framebuffers().next()) {
        Some(fb) => match fb::init(&fb) {
            Ok((w, h)) => info!(target: "fb", "framebuffer {w}x{h} ready"),
            Err(e) => warn!(target: "fb", "framebuffer {}x{} unusable: {e}", fb.width(), fb.height()),
        },
        None => warn!(target: "fb", "no framebuffer"),
    }
    if !arch::serial::present() {
        // Without serial only the screen's copy of the log remains: info and
        // above, from the framebuffer on (debug, trace and earlier lines are
        // lost).
        warn!(target: "boot", "no serial port at COM1: logging to the screen only");
    }
    debug!(target: "serial", "COM1: {:?}", arch::serial::mode());
    ktrace!(Kind::BootPhase, BootPhase::Framebuffer);
    keyboard::init();
    // Drivers may wait (bounded) and start threads, so after the
    // scheduler and the clocks; before the shells.
    drivers::probe_all();
    // Randomness: RDRAND, and the virtio-rng the probe brought up.
    rng::init();

    // Keep these three together and in this order (`boot complete`, the
    // complete mark, the timeline's line): test_boot_timeline_printed checks
    // that the total falls between the two lines' stamps. Work between them
    // weakens that check without failing it. Moving the timeline's line
    // above the mark fails it; swapping `boot complete` with the mark fails
    // it under TCG, but under KVM their reads may round to the same
    // microsecond and pass.
    info!(target: "boot", "boot complete");
    ktrace!(Kind::BootPhase, BootPhase::Complete);
    let timeline = trace::Timeline::boot();
    if timeline.is_complete() {
        info!(target: "boot", "timeline: {timeline}");
    } else {
        warn!(target: "boot", "timeline: {timeline}");
    }
    log_before_kmain();
    after_boot()
}

/// Limine's times for the boot and where the timeline's origin falls after
/// its handoff (`lumen_time::handoff`). A warning if any of it is unknown
/// (a missing Limine response, Limine's 0 Hz, no TSC calibration or no
/// origin) or out of order (the reason follows the line).
fn log_before_kmain() {
    match boot::before_kmain() {
        Err(why) => warn!(target: "boot", "before kmain: unknown ({why})"),
        Ok(before) => match before.check() {
            Ok(()) => info!(target: "boot", "before kmain: {before}"),
            Err(disorder) => warn!(target: "boot", "before kmain: {before}; out of order: {disorder}"),
        },
    }
}

#[cfg(feature = "ktest")]
fn after_boot() -> ! {
    let ok = ktest::run_all();
    if cmdline::qemu_exit_enabled() {
        let code = if ok { arch::qemu::ExitCode::Success } else { arch::qemu::ExitCode::Failure };
        arch::qemu::exit(code);
    }
    arch::halt_forever()
}

#[cfg(not(feature = "ktest"))]
fn after_boot() -> ! {
    shell::run()
}

fn log_memory_map() {
    let Some(resp) = boot::MEMMAP.get_response() else {
        warn!(target: "mem", "no memory map");
        return;
    };
    let mut usable = 0u64;
    for e in resp.entries() {
        log::debug!(
            target: "mem",
            "{:#014x}-{:#014x} {}",
            e.base,
            e.base + e.length,
            boot::memmap_type_name(e.entry_type)
        );
        if e.entry_type == limine::memory_map::EntryType::USABLE {
            usable += e.length;
        }
    }
    info!(target: "mem", "{} regions, {} MiB usable", resp.entries().len(), usable / (1024 * 1024));
}

/// The bytes of a file Limine loaded for us.
fn file_bytes(file: &limine::file::File) -> &'static [u8] {
    // SAFETY: the file is in the HHDM (its memory-map entry type is one of
    // `mm::paging::HHDM_TYPES`, which Limine's tables and then the kernel's
    // map for the kernel's whole life), and the frame allocator never hands
    // it out (non-usable memory, plus the `kernel file` reservation).
    unsafe { core::slice::from_raw_parts(file.addr(), file.size() as usize) }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    crash::panic(info)
}

crate::ktest!(
    fn gdt_and_idt_are_ours() {
        crate::kassert_eq!(arch::gdt::current_cs(), arch::gdt::KERNEL_CODE);
        crate::kassert_eq!(arch::idt::loaded_base(), arch::idt::our_base());
        Ok(())
    }
);

crate::ktest!(
    fn breakpoint_exception_returns() {
        use core::sync::atomic::Ordering;
        let before = arch::idt::BREAKPOINTS.load(Ordering::Relaxed);
        let canary: u64 = core::hint::black_box(0x1234_5678_9abc_def0);
        // SAFETY: #BP is handled and resumes; this checks the entry stub restores
        // every register.
        unsafe { core::arch::asm!("int3") };
        crate::kassert_eq!(arch::idt::BREAKPOINTS.load(Ordering::Relaxed), before + 1);
        crate::kassert_eq!(core::hint::black_box(canary), 0x1234_5678_9abc_def0);
        Ok(())
    }
);
