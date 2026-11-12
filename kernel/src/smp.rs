//! Bringing up the other CPUs (application processors, APs).
//!
//! Limine starts every CPU and parks the APs, each spinning on its
//! `goto_address` with Limine's page tables, GDT and a 64 KiB (or requested)
//! stack. [`start_all`], on the boot CPU right after the interrupt
//! controllers are set up, starts them all:
//!
//! 1. it allocates the AP's main stack and its double-fault, NMI and
//!    machine-check stacks (each with a guard page, hard rule 4) and puts the
//!    latter in the AP's TSS;
//! 2. it hands the AP its CPU index and jumps it to [`ap_entry`], which loads
//!    the AP's own GDT/TSS, per-CPU record and the IDT first (hard rule 6),
//!    turns on NX and WP, then switches to the kernel's page tables and its
//!    allocated stack in one step. Nothing in that stretch can fault (its
//!    only checks are bounds checks `start`'s indices always pass): there an
//!    NMI, #DF or #MC would find the AP's IST stacks unmapped (they are in the
//!    kernel's tables only), a triple fault, and any other exception would
//!    lose its report to a fault on the crash reporter's local APIC write;
//! 3. on its own stack the AP enables its local APIC, checks NX, the PAT and
//!    its APIC id (a failure is reported normally), logs
//!    `cpu N online (lapic id X)` and idles with interrupts on, running
//!    whatever [`run_on`] hands it (kmon's `crash <kind> [cpu]`);
//! 4. after calibrating the clocks, the boot CPU waits (bounded, 2 s in all)
//!    for every AP to be online ([`wait_online`]).
//!
//! Starting them before the calibration matters: until an AP is started it
//! spins in Limine's park loop, taking host time from every guest
//! calibrating alongside under emulation (docs/ARCHITECTURE.md, "Other
//! CPUs", has the measurements). Once started, an AP halts.
//!
//! CPUs past [`MAX_CPUS`](crate::arch::MAX_CPUS) are halted for good, with a
//! warning (left in Limine, each would spin on its `goto_address` forever).
//! An AP never handles device interrupts (every I/O APIC input targets the
//! boot CPU, and its local APIC timer stays masked).

use core::mem::size_of;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use limine::mp::Cpu;
use log::{info, warn};
use lumen_mm::stacks::{StackKind, StackRange};

use crate::arch::{self, MAX_CPUS};
use crate::mm::stack::{KernelStack, MAIN_STACK_SIZE, STACK_SIZE};
use crate::{boot, mm, time};

/// How long the boot CPU waits, in all, for every AP to come online and
/// every CPU past the cap to halt.
const START_TIMEOUT_NS: u64 = 2_000_000_000;

// Four stacks a CPU (main, #DF, NMI, #MC), and room left for others.
const _: () = assert!(4 * MAX_CPUS < lumen_mm::stacks::MAX_STACKS);

/// Where a CPU's slot is in its life. It only moves forward (Empty,
/// Starting, Online; the boot CPU straight to Online), which the lock-free
/// readers rely on: once online, a CPU stays online.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    /// No CPU has this index.
    Empty = 0,
    /// Started (stacks set, about to jump to `ap_entry`), not online yet.
    Starting = 1,
    /// Running the kernel on its own stack with its local APIC enabled
    /// (and, so, its own IDT and TSS: an NMI is safe to send). The boot CPU
    /// is online from [`start_all`] on.
    Online = 2,
}

/// A job an AP's idle loop runs: a `fn()` kept as a usize, 0 if none (a
/// function pointer is never null).
struct Mailbox(AtomicUsize);

const _: () = assert!(size_of::<fn()>() == size_of::<usize>());

impl Mailbox {
    const fn new() -> Mailbox {
        Mailbox(AtomicUsize::new(0))
    }

    /// Put `job` in, unless one is already waiting.
    #[must_use]
    fn offer(&self, job: fn()) -> bool {
        self.0.compare_exchange(0, job as usize, Ordering::Release, Ordering::Relaxed).is_ok()
    }

    fn is_full(&self) -> bool {
        self.0.load(Ordering::Relaxed) != 0
    }

    #[must_use]
    fn take(&self) -> Option<fn()> {
        match self.0.swap(0, Ordering::Acquire) {
            0 => None,
            // SAFETY: `offer` is the only store of a non-zero value, and it
            // stores a `fn()`.
            raw => Some(unsafe { core::mem::transmute::<usize, fn()>(raw) }),
        }
    }
}

/// One per CPU index.
struct Slot {
    /// The top of its allocated main stack (APs).
    main_top: AtomicU64,
    /// Its local APIC id: from Limine (the MADT's) for an AP, read from its
    /// own local APIC for the boot CPU. Valid unless the state is `Empty`.
    lapic_id: AtomicU32,
    state: AtomicU8,
    job: Mailbox,
}

impl Slot {
    fn state(&self) -> State {
        match self.state.load(Ordering::Acquire) {
            1 => State::Starting,
            2 => State::Online,
            _ => State::Empty,
        }
    }

    fn set(&self, state: State) {
        let old = self.state.swap(state as u8, Ordering::AcqRel);
        debug_assert!(old < state as u8, "a CPU's state only moves forward");
    }
}

static SLOTS: [Slot; MAX_CPUS] = [const {
    Slot {
        main_top: AtomicU64::new(0),
        lapic_id: AtomicU32::new(0),
        state: AtomicU8::new(State::Empty as u8),
        job: Mailbox::new(),
    }
}; MAX_CPUS];

/// How many CPUs past `MAX_CPUS` have reached [`park`].
static HALTED: AtomicUsize = AtomicUsize::new(0);
/// The kernel's page-table root, for the APs (read lock-free); nonzero once
/// [`start_all`] has run.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

/// CPU `index`'s local APIC id, if it is online (it can take IPIs).
/// Lock-free (the crash reporter stops exactly these CPUs).
pub fn online_lapic_id(index: usize) -> Option<u32> {
    let slot = SLOTS.get(index)?;
    (slot.state() == State::Online).then(|| slot.lapic_id.load(Ordering::Relaxed))
}

/// Has CPU `index` been started but isn't online yet? (The crash reporter
/// can't stop such a CPU, and says so.)
pub fn is_starting(index: usize) -> bool {
    SLOTS.get(index).is_some_and(|s| s.state() == State::Starting)
}

/// How many CPUs are online (the boot CPU included).
fn online() -> usize {
    SLOTS.iter().filter(|s| s.state() == State::Online).count()
}

/// Why [`run_on`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunError {
    /// That CPU isn't an online AP (it doesn't exist, didn't start, or is the
    /// boot CPU, which runs kmon itself).
    NotAnOnlineAp,
    /// It hasn't taken the last job yet.
    Busy,
    /// Its wakeup IPI couldn't be sent (this CPU's local APIC stayed busy);
    /// the job was taken back.
    IpiNotSent,
}

/// How kmon finishes `crash: cpu N ...` (a test interface).
impl core::fmt::Display for RunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            RunError::NotAnOnlineAp => "isn't online",
            RunError::Busy => "hasn't taken its last job yet",
            RunError::IpiNotSent => "couldn't be woken (its IPI wasn't sent)",
        })
    }
}

/// Have AP `index` run `job` from its idle loop (on its own stack, with
/// interrupts on), and wake it. Returns once the job is handed over, not
/// when it has run.
pub fn run_on(index: usize, job: fn()) -> Result<(), RunError> {
    let Some(lapic) = online_lapic_id(index).filter(|_| index != 0) else {
        return Err(RunError::NotAnOnlineAp);
    };
    let slot = &SLOTS[index];
    if !slot.job.offer(job) {
        return Err(RunError::Busy);
    }
    // A late IPI may still wake it: if it has taken the job, it runs it.
    if !arch::apic::send_ipi(lapic, arch::idt::WAKEUP) && slot.job.take().is_some() {
        return Err(RunError::IpiNotSent);
    }
    Ok(())
}

/// What [`start_all`] did, for [`wait_online`].
#[must_use = "wait_online needs it"]
pub struct Started {
    /// APs started (indices 1..=aps).
    aps: usize,
    /// CPUs past the cap sent to `park`.
    parked: usize,
}

/// Start every other CPU Limine reports, up to `MAX_CPUS` in all (the rest
/// are halted), without waiting for them. Once, on the boot CPU, once the
/// local APIC is mapped (`interrupts::init`), before `time::init`.
pub fn start_all() -> Started {
    let root = mm::paging::kernel_root().as_u64();
    assert!(KERNEL_ROOT.swap(root, Ordering::AcqRel) == 0, "smp::start_all called twice");
    let bsp = arch::apic::lapic_id();
    SLOTS[0].lapic_id.store(bsp, Ordering::Relaxed);
    SLOTS[0].set(State::Online);
    let mut started = Started { aps: 0, parked: 0 };
    let Some(resp) = boot::MP.get_response() else {
        warn!(target: "smp", "no MP response from the bootloader: only the boot CPU runs");
        return started;
    };
    assert_eq!(resp.bsp_lapic_id(), bsp, "Limine and the local APIC disagree on the boot CPU's APIC id");
    for cpu in resp.cpus().iter().filter(|c| c.lapic_id != bsp) {
        if started.aps + 1 == MAX_CPUS {
            cpu.goto_address.write(park);
            started.parked += 1;
        } else {
            started.aps += 1;
            start(cpu, started.aps);
        }
    }
    started
}

/// Spin until `done()` or the deadline (`time::now_ns`); whether `done()`.
fn spin_until(deadline: u64, done: impl Fn() -> bool) -> bool {
    while !done() {
        if time::now_ns() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

/// Wait (bounded) until every AP [`start_all`] started is online and every
/// CPU past the cap has halted, and say how many CPUs are online. Once, on
/// the boot CPU, after `time::init` (it times the wait). An AP that doesn't
/// make it is warned about; it keeps its index and stacks (it may still be
/// on its way).
pub fn wait_online(started: Started) {
    let deadline = time::now_ns() + START_TIMEOUT_NS;
    for (index, slot) in SLOTS[..=started.aps].iter().enumerate().skip(1) {
        if !spin_until(deadline, || slot.state() == State::Online) {
            let lapic = slot.lapic_id.load(Ordering::Relaxed);
            warn!(target: "smp", "cpu {index} (lapic id {lapic}) didn't come online");
        }
    }
    if started.parked > 0 {
        let parked = started.parked;
        if spin_until(deadline, || HALTED.load(Ordering::Acquire) >= parked) {
            warn!(target: "smp", "{parked} CPU(s) past the limit halted: the kernel runs on at most {MAX_CPUS}");
        } else {
            let halted = HALTED.load(Ordering::Acquire);
            warn!(target: "smp", "only {halted} of {parked} CPU(s) past the limit halted (the kernel runs on at most {MAX_CPUS})");
        }
    }
    let (n, expected) = (online(), started.aps + 1);
    if n == expected {
        info!(target: "smp", "{n} CPU{} online", if n == 1 { "" } else { "s" });
    } else {
        warn!(target: "smp", "{n} of {expected} CPUs online");
    }
}

fn allocate(size: u64, kind: StackKind, index: usize) -> StackRange {
    KernelStack::new(size, kind)
        .unwrap_or_else(|e| panic!("allocating cpu {index}'s {} stack: {e}", kind.name()))
        .leak()
}

/// Start `cpu` as CPU `index`.
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
    slot.set(State::Starting);
    cpu.extra.store(index as u64, Ordering::Relaxed);
    // A SeqCst store: everything above is visible to the AP before it jumps.
    cpu.goto_address.write(ap_entry);
}

/// Where a CPU past `MAX_CPUS` goes: count itself and halt (interrupts off)
/// on Limine's tables and stack, touching nothing else of the kernel's.
/// Nothing sends it an IPI (it isn't online). (The count shows it got here,
/// right before the halt.)
unsafe extern "C" fn park(_cpu: &Cpu) -> ! {
    HALTED.fetch_add(1, Ordering::Release);
    arch::halt_forever()
}

/// Where an AP starts: on Limine's page tables and stack, interrupts off.
/// Nothing here may fault (see the module docs): no check that can fail
/// (the index `start` passed is below `MAX_CPUS`), only loads, stores and
/// register writes, then the switch.
unsafe extern "C" fn ap_entry(cpu: &Cpu) -> ! {
    let index = cpu.extra.load(Ordering::Relaxed) as usize;
    // SAFETY: `start` gave this CPU its own index and filled its IST slots
    // before jumping it here; Limine starts APs with interrupts off.
    unsafe { arch::init_ap(index) };
    // The kernel's entries use NX, reserved without EFER.NXE.
    // SAFETY: the boot CPU checked NX support (`mm::paging::init`), and every
    // CPU of the machine is the same model; `ap_main` checks again.
    unsafe { arch::paging::turn_on_nx_and_wp() };
    let root = KERNEL_ROOT.load(Ordering::Acquire);
    let top = SLOTS[index].main_top.load(Ordering::Relaxed);
    // SAFETY: `root` is the boot CPU's live root, which maps the whole kernel
    // (and this AP's allocated stack); NXE is on (just above); the stack is
    // this AP's alone, leaked, and page-aligned (so 16-byte aligned).
    unsafe { arch::enter_kernel_space(root, top, ap_main) }
}

/// An AP's life on the kernel's tables and its own stack.
extern "C" fn ap_main() -> ! {
    let index = arch::cpu_index();
    let slot = &SLOTS[index];
    arch::apic::enable_local();
    // The checks `ap_entry` couldn't afford: a failure here is reported.
    arch::paging::enforce_nx_and_wp();
    arch::paging::check_pat();
    let lapic = arch::apic::lapic_id();
    assert_eq!(
        lapic,
        slot.lapic_id.load(Ordering::Relaxed),
        "cpu {index}'s local APIC id isn't the one Limine reported"
    );
    info!(target: "smp", "cpu {index} online (lapic id {lapic})");
    slot.set(State::Online);
    loop {
        // Checked with interrupts off, so a wakeup can't be lost.
        arch::irq::wait_unless(|| slot.job.is_full());
        if let Some(job) = slot.job.take() {
            job();
        }
    }
}
