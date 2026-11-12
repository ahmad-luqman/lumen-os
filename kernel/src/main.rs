//! The Lumen kernel.
//!
//! Boot order (see docs/ARCHITECTURE.md, "Boot"):
//! 1. `_start` (naked): clear rbp so backtraces end here, align the stack.
//! 2. `arch::init_early`: serial, GDT/TSS (IST stacks), IDT. From here on any
//!    CPU exception produces a crash report.
//! 3. Logger, Limine responses, symbol table, memory map, framebuffer.
//! 4. Either the in-kernel tests (`--features ktest`) or the kernel monitor.

#![no_std]
#![no_main]

mod arch;
mod boot;
mod cmdline;
mod crash;
mod fb;
mod klog;
#[cfg_attr(feature = "ktest", allow(dead_code))] // replaced by the test runner
mod kmon;
mod ktest;
mod mm;
mod symbols;
mod sync;

use log::{LevelFilter, info, warn};

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
    arch::init_early();
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

    if let Some(addr) = boot::EXECUTABLE_ADDRESS.get_response() {
        info!(target: "boot", "kernel at phys {:#x} virt {:#x}", addr.physical_base(), addr.virtual_base());
    }
    if let Some(hhdm) = boot::HHDM.get_response() {
        info!(target: "boot", "hhdm offset {:#x}", hhdm.offset());
    }
    log_memory_map();
    mm::init();

    match boot::FRAMEBUFFER.get_response().and_then(|r| r.framebuffers().next()) {
        Some(fb) => match fb::init(&fb) {
            Some((w, h)) => info!(target: "fb", "framebuffer {w}x{h} ready"),
            None => warn!(target: "fb", "framebuffer format unsupported (bpp {})", fb.bpp()),
        },
        None => warn!(target: "fb", "no framebuffer"),
    }

    info!(target: "boot", "boot complete");
    after_boot()
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
    kmon::run()
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
    // SAFETY: Limine maps the file in the HHDM for the life of the kernel
    // (bootloader-reclaimable memory, which the frame allocator keeps reserved).
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
