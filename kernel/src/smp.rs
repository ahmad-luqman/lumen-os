//! Bringing up the other CPUs (application processors, APs).
//!
//! Limine starts every CPU and parks the APs, each spinning on its
//! `goto_address` with Limine's page tables, GDT and a 64 KiB (or requested)
//! stack. [`init`], on the boot CPU, starts them one at a time:
//!
//! 1. it allocates the AP's main stack and its double-fault, NMI and
//!    machine-check stacks (each with a guard page, hard rule 4) and puts the
//!    latter in the AP's TSS;
//! 2. it hands the AP its CPU index and jumps it to [`ap_entry`], which loads
//!    the AP's own GDT/TSS, per-CPU record and the IDT first (hard rule 6),
//!    makes the CPU honour NX and WP, then switches to the kernel's page
//!    tables and its allocated stack in one step;
//! 3. on that stack the AP enables its local APIC, logs
//!    `cpu N online (lapic id X)` and idles with interrupts on;
//! 4. the boot CPU waits (bounded) for that before starting the next.
//!
//! CPUs past [`MAX_CPUS`](crate::arch::MAX_CPUS) are halted for good, with a
//! warning (left in Limine, each would spin on its `goto_address` forever). An AP never handles device interrupts (every I/O APIC
//! input targets the boot CPU, and its local APIC timer stays masked).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use limine::mp::Cpu;
use log::{info, warn};
use lumen_mm::stacks::{StackKind, StackRange};

use crate::arch::{self, MAX_CPUS};
use crate::mm::stack::{KernelStack, MAIN_STACK_SIZE, STACK_SIZE};
use crate::{boot, mm, time};

/// How long the boot CPU waits for an AP to say it is online.
const START_TIMEOUT_NS: u64 = 2_000_000_000;

/// What an AP needs to start, and whether it has (index = CPU index).
struct Slot {
    /// The top of its allocated main stack.
    main_top: AtomicU64,
    /// Its local APIC id, from Limine (the MADT's).
    lapic_id: AtomicU32,
    /// Set by the CPU itself once it runs on its own stack with its local
    /// APIC enabled.
    online: AtomicBool,
}

static SLOTS: [Slot; MAX_CPUS] =
    [const { Slot { main_top: AtomicU64::new(0), lapic_id: AtomicU32::new(0), online: AtomicBool::new(false) } };
        MAX_CPUS];

/// CPUs online, the boot CPU included.
static ONLINE: AtomicUsize = AtomicUsize::new(1);
/// The kernel's page-table root, for the APs (read lock-free).
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

/// How many CPUs are online (the boot CPU included).
pub fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// CPU `index`'s local APIC id (set for the boot CPU by [`init`], for an AP
/// when it is started).
pub fn lapic_id(index: usize) -> u32 {
    SLOTS[index].lapic_id.load(Ordering::Relaxed)
}

/// Start every other CPU Limine reports, up to `MAX_CPUS` in all. Runs once,
/// on the boot CPU, after `time::init` (it times the wait) and with
/// interrupts and the local APIC set up.
pub fn init() {
    let bsp = arch::apic::lapic_id();
    SLOTS[0].lapic_id.store(bsp, Ordering::Relaxed);
    let Some(resp) = boot::MP.get_response() else {
        warn!(target: "smp", "no MP response from the bootloader: only the boot CPU runs");
        return;
    };
    assert_eq!(resp.bsp_lapic_id(), bsp, "Limine and the local APIC disagree on the boot CPU's APIC id");
    KERNEL_ROOT.store(mm::paging::kernel_root().as_u64(), Ordering::Release);
    let mut next = 1;
    let mut parked = 0usize;
    for cpu in resp.cpus().iter().filter(|c| c.lapic_id != bsp) {
        if next == MAX_CPUS {
            cpu.goto_address.write(park);
            parked += 1;
            continue;
        }
        start(cpu, next);
        // An AP that didn't come online in time keeps its index and stacks:
        // it may still be on its way.
        next += 1;
    }
    if parked > 0 {
        warn!(target: "smp", "{parked} CPU(s) halted: the kernel runs on at most {MAX_CPUS}");
    }
    let n = online();
    info!(target: "smp", "{n} CPU{} online", if n == 1 { "" } else { "s" });
}

fn allocate(size: u64, kind: StackKind, index: usize) -> StackRange {
    KernelStack::new(size, kind)
        .unwrap_or_else(|e| panic!("allocating cpu {index}'s {} stack: {e}", kind.name()))
        .leak()
}

/// Start `cpu` as CPU `index` and wait for it to come online.
fn start(cpu: &Cpu, index: usize) {
    let df = allocate(STACK_SIZE, StackKind::DoubleFault, index);
    let nmi = allocate(STACK_SIZE, StackKind::Nmi, index);
    let mc = allocate(STACK_SIZE, StackKind::MachineCheck, index);
    let main = allocate(MAIN_STACK_SIZE, StackKind::Main, index);
    // SAFETY: leaked, mapped stacks nothing else uses; the AP is parked in
    // Limine and runs no kernel code until `goto_address` is written below.
    unsafe { arch::set_ap_ist_stacks(index, df.top, nmi.top, mc.top) };
    info!(
        target: "smp",
        "cpu {index} stacks: main {:#x}..{:#x}, IST {:#x} {:#x} {:#x} (guard page below each)",
        main.bottom,
        main.top,
        df.bottom,
        nmi.bottom,
        mc.bottom
    );
    let slot = &SLOTS[index];
    slot.main_top.store(main.top, Ordering::Relaxed);
    slot.lapic_id.store(cpu.lapic_id, Ordering::Relaxed);
    cpu.extra.store(index as u64, Ordering::Relaxed);
    // A SeqCst store: everything above is visible to the AP before it jumps.
    cpu.goto_address.write(ap_entry);

    let deadline = time::now_ns() + START_TIMEOUT_NS;
    while !slot.online.load(Ordering::Acquire) {
        if time::now_ns() > deadline {
            warn!(target: "smp", "cpu {index} (lapic id {}) didn't come online", cpu.lapic_id);
            return;
        }
        core::hint::spin_loop();
    }
}

/// Where a CPU past `MAX_CPUS` goes: halt (interrupts off) on Limine's
/// tables and stack, touching nothing of the kernel's. Nothing sends it an
/// IPI (it isn't online).
unsafe extern "C" fn park(_cpu: &Cpu) -> ! {
    arch::halt_forever()
}

/// Where an AP starts: on Limine's page tables and stack, interrupts off.
unsafe extern "C" fn ap_entry(cpu: &Cpu) -> ! {
    let index = cpu.extra.load(Ordering::Relaxed) as usize;
    // SAFETY: `start` gave this CPU its own index and filled its IST slots
    // before jumping it here; Limine starts APs with interrupts off.
    unsafe { arch::init_ap(index) };
    // Before the kernel's tables, whose entries use NX (reserved without
    // EFER.NXE), and the PAT slots their memory types rely on.
    arch::paging::enforce_nx_and_wp();
    arch::paging::check_pat();
    let root = KERNEL_ROOT.load(Ordering::Acquire);
    let top = SLOTS[index].main_top.load(Ordering::Relaxed);
    // SAFETY: `root` is the boot CPU's live root, which maps the whole kernel
    // (and this AP's allocated stack); NXE is on (just above); the stack is
    // this AP's alone and leaked.
    unsafe { arch::enter_kernel_space(root, top, ap_main) }
}

/// An AP's life on the kernel's tables and its own stack.
extern "C" fn ap_main() -> ! {
    let index = arch::cpu_index();
    arch::apic::enable_local();
    let lapic = arch::apic::lapic_id();
    assert_eq!(lapic, lapic_id(index), "cpu {index}'s local APIC id isn't the one Limine reported");
    info!(target: "smp", "cpu {index} online (lapic id {lapic})");
    ONLINE.fetch_add(1, Ordering::AcqRel);
    SLOTS[index].online.store(true, Ordering::Release);
    loop {
        arch::wait_for_interrupt();
    }
}
