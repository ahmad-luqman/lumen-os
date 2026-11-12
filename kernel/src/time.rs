//! Timekeeping: the TSC's frequency, calibrated at boot against the HPET (or
//! the PIT if there is none), [`now_ns`] and [`sleep`] from it, and the local
//! APIC timer ticking at [`TICK_HZ`], calibrated against the TSC.
//!
//! Calibration polls with interrupts off and takes the median of several
//! 10 ms windows, so one window stretched by an SMI or a descheduled vCPU
//! doesn't skew it. [`now_ns`] and [`ticks`] are lock-free (interrupt
//! handlers and the crash path may call them).

use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use log::info;
use lumen_mm::PhysAddr;
use lumen_time::{Clock, Hz, NS_PER_SEC, PIT_HZ};

use crate::arch::{self, apic, idt, pit};
use crate::mm::paging;

/// The local APIC timer's rate.
pub const TICK_HZ: u64 = 1000;
/// Calibration windows, and how long each is.
const WINDOWS: usize = 5;
const WINDOW_NS: u64 = 10_000_000;

/// The TSC's frequency (0 until [`init`]).
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// The local APIC timer's input frequency, after its divider (0 until
/// [`init`]).
static LAPIC_TIMER_HZ: AtomicU64 = AtomicU64::new(0);
/// What the TSC was calibrated against: 0 not yet, else a [`Reference`]
/// plus 1.
static REFERENCE: AtomicU8 = AtomicU8::new(0);

/// What the TSC was calibrated against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reference {
    Hpet,
    Pit,
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reference::Hpet => "hpet",
            Reference::Pit => "pit",
        })
    }
}

/// What the TSC was calibrated against at boot, once it was.
pub fn reference() -> Option<Reference> {
    match REFERENCE.load(Ordering::Relaxed) {
        1 => Some(Reference::Hpet),
        2 => Some(Reference::Pit),
        _ => None,
    }
}

/// Measure the TSC's frequency again now, against `reference` (with
/// interrupts off meanwhile: about 50 ms). `None` if that clock is missing
/// or doesn't count. For cross-checking the boot calibration.
pub fn measure(reference: Reference) -> Option<u64> {
    arch::irq::push_off();
    let hz = match reference {
        Reference::Hpet => hpet().and_then(|h| calibrate_with_hpet(&h)),
        Reference::Pit => calibrate_with_pit(),
    };
    arch::irq::pop_off();
    hz
}

/// The HPET's main counter.
pub struct HpetCounter {
    regs: u64,
    hz: u64,
    /// The counter is 32 bits wide: deltas wrap at 2^32.
    narrow: bool,
}

const HPET_CAPABILITIES: usize = 0x00;
const HPET_CONFIG: usize = 0x10;
const HPET_COUNTER: usize = 0xf0;
const HPET_ENABLE: u64 = 1 << 0;
const HPET_COUNT_64: u64 = 1 << 13;

impl HpetCounter {
    /// Map the HPET at `phys`, start its counter if it isn't running, and
    /// read its frequency. `None` if its period is out of the spec's range.
    fn new(phys: u64) -> Option<HpetCounter> {
        let virt = paging::map_mmio(PhysAddr::new(phys)?, 1024).ok()?.as_u64();
        let mut hpet = HpetCounter { regs: virt, hz: 0, narrow: false };
        let caps = hpet.read(HPET_CAPABILITIES);
        hpet.hz = lumen_time::hpet_hz((caps >> 32) as u32)?;
        hpet.narrow = caps & HPET_COUNT_64 == 0;
        let config = hpet.read(HPET_CONFIG);
        if config & HPET_ENABLE == 0 {
            hpet.write(HPET_CONFIG, config | HPET_ENABLE);
        }
        Some(hpet)
    }

    fn read(&self, offset: usize) -> u64 {
        // SAFETY: `regs` maps the HPET's registers (1 KiB, uncached) for
        // good; these are 64-bit registers read with 64-bit accesses.
        unsafe { ((self.regs as usize + offset) as *const u64).read_volatile() }
    }

    fn write(&self, offset: usize, value: u64) {
        // SAFETY: as for `read`; only the general configuration is written.
        unsafe { ((self.regs as usize + offset) as *mut u64).write_volatile(value) }
    }

    pub fn now(&self) -> u64 {
        let v = self.read(HPET_COUNTER);
        if self.narrow { v & 0xffff_ffff } else { v }
    }

    /// Ticks from `earlier` to `later` (two [`now`](Self::now) readings).
    pub fn since(&self, earlier: u64, later: u64) -> u64 {
        if self.narrow { later.wrapping_sub(earlier) & 0xffff_ffff } else { later.wrapping_sub(earlier) }
    }

    pub fn hz(&self) -> u64 {
        self.hz
    }
}

/// The HPET ACPI describes, if there is one and it works.
pub fn hpet() -> Option<HpetCounter> {
    let hpet = crate::acpi::info()?.hpet?;
    HpetCounter::new(hpet.address)
}

/// The TSC's frequency against the HPET: the median of [`WINDOWS`] windows.
fn calibrate_with_hpet(hpet: &HpetCounter) -> Option<u64> {
    let target = hpet.hz() * WINDOW_NS / NS_PER_SEC;
    let mut samples = [0u64; WINDOWS];
    for s in &mut samples {
        let (h0, t0) = (hpet.now(), arch::rdtsc());
        let (h1, t1) = loop {
            let (h, t) = (hpet.now(), arch::rdtsc());
            if hpet.since(h0, h) >= target {
                break (h, t);
            }
            core::hint::spin_loop();
        };
        *s = lumen_time::frequency(t1 - t0, hpet.since(h0, h1), hpet.hz())?;
    }
    lumen_time::median(&mut samples)
}

/// The TSC's frequency against the PIT's channel 2: the median of
/// [`WINDOWS`] windows. `None` if the PIT doesn't count.
pub fn calibrate_with_pit() -> Option<u64> {
    let count = lumen_time::pit_count(WINDOW_NS).expect("a 10 ms window fits the PIT");
    let mut samples = [0u64; WINDOWS];
    for s in &mut samples {
        let mut t0 = 0;
        if !pit::wait(count, || t0 = arch::rdtsc()) {
            return None;
        }
        *s = lumen_time::frequency(arch::rdtsc() - t0, u64::from(count), PIT_HZ)?;
    }
    lumen_time::median(&mut samples)
}

/// The local APIC timer's input frequency against the (calibrated) TSC.
fn calibrate_lapic_timer(tsc: Clock) -> Option<u64> {
    let window = tsc.ticks(WINDOW_NS);
    let mut samples = [0u64; WINDOWS];
    for s in &mut samples {
        apic::timer_one_shot(u32::MAX);
        let t0 = arch::rdtsc();
        while arch::rdtsc() - t0 < window {
            core::hint::spin_loop();
        }
        let (counted, t1) = (u32::MAX - apic::timer_current(), arch::rdtsc());
        *s = lumen_time::frequency(u64::from(counted), t1 - t0, tsc.hz())?;
    }
    lumen_time::median(&mut samples)
}

/// Calibrate the TSC and start the local APIC timer at [`TICK_HZ`] (its
/// interrupts arrive once interrupts are enabled). Needs `interrupts::init`;
/// call with interrupts off. Panics if no clock can calibrate the TSC.
pub fn init() {
    assert!(!arch::irq::enabled(), "time::init with interrupts on");
    let (hz, reference) = match hpet().and_then(|h| calibrate_with_hpet(&h)) {
        Some(hz) => (hz, Reference::Hpet),
        None => (calibrate_with_pit().expect("can't calibrate the TSC: no HPET and no working PIT"), Reference::Pit),
    };
    TSC_HZ.store(hz, Ordering::Relaxed);
    REFERENCE.store(reference as u8 + 1, Ordering::Relaxed);
    info!(target: "time", "tsc {} (calibrated against {reference})", Hz(hz));

    let tsc = Clock::new(hz).expect("calibrated");
    let timer_hz = calibrate_lapic_timer(tsc).expect("the local APIC timer doesn't count");
    let count = lumen_time::periodic_count(timer_hz, TICK_HZ).expect("the local APIC timer is too slow for 1 kHz");
    LAPIC_TIMER_HZ.store(timer_hz, Ordering::Relaxed);
    apic::timer_periodic(count, idt::TIMER);
    info!(
        target: "time",
        "local APIC timer {} (bus {} / {}), ticking at {}",
        Hz(timer_hz),
        Hz(timer_hz * apic::TIMER_DIVISOR),
        apic::TIMER_DIVISOR,
        Hz(TICK_HZ)
    );
}

/// The TSC as a [`Clock`], once calibrated.
pub fn tsc() -> Option<Clock> {
    Clock::new(TSC_HZ.load(Ordering::Relaxed))
}

/// Nanoseconds since the TSC started (CPU reset), or 0 before [`init`].
/// Lock-free.
pub fn now_ns() -> u64 {
    tsc().map_or(0, |c| c.ns(arch::rdtsc()))
}

/// Wait at least `ns` nanoseconds (busy: the scheduler comes in M2).
/// Panics before [`init`].
pub fn sleep(ns: u64) {
    let tsc = tsc().expect("time::sleep before time::init");
    let start = arch::rdtsc();
    let ticks = tsc.ticks(ns);
    while arch::rdtsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// Local APIC timer interrupts so far. Lock-free.
pub fn ticks() -> u64 {
    idt::TICKS.load(Ordering::Relaxed)
}

crate::ktest!(
    fn tsc_calibrated_within_tolerance() {
        // Cross-check against a different clock: the PIT if the TSC was
        // calibrated against the HPET (and the HPET otherwise). Within 2%.
        use crate::kassert;
        let hz = tsc().ok_or("TSC not calibrated")?.hz();
        let other = match reference() {
            Some(Reference::Hpet) => measure(Reference::Pit).ok_or("the PIT doesn't count")?,
            _ => return Err("the TSC wasn't calibrated against the HPET"),
        };
        crate::ktest::diag(format_args!("tsc {} (boot), {} against the pit", Hz(hz), Hz(other)));
        kassert!(hz.abs_diff(other) * 50 <= hz);
        Ok(())
    }
);

crate::ktest!(
    fn sleep_10ms_elapses() {
        // Measured with the HPET and the tick count, not the TSC that
        // `sleep` itself waits on.
        use crate::kassert;
        let hpet = hpet().ok_or("no HPET")?;
        let (h0, t0) = (hpet.now(), ticks());
        sleep(10_000_000);
        let (elapsed, ticked) = (hpet.since(h0, hpet.now()), ticks() - t0);
        let ns = Clock::new(hpet.hz()).ok_or("hpet")?.ns(elapsed);
        crate::ktest::diag(format_args!("slept {ns} ns by the HPET, {ticked} ticks"));
        // At least 10 ms, less 1% for the TSC's calibration error; well
        // under a second; and the timer ticked (interrupts are on).
        kassert!((9_900_000..NS_PER_SEC).contains(&ns));
        kassert!(ticked >= 5);
        Ok(())
    }
);

crate::ktest!(
    fn the_timer_ticks_at_1_khz_on_its_vector() {
        use crate::{kassert, kassert_eq};
        kassert!(arch::irq::enabled());
        // Periodic, unmasked, on the timer vector.
        let lvt = apic::timer_lvt();
        kassert_eq!((lvt & 0xff, lvt & (1 << 16), lvt & (1 << 17)), (u32::from(idt::TIMER), 0, 1 << 17));
        let t0 = ticks();
        sleep(100_000_000);
        let ticked = ticks() - t0;
        crate::ktest::diag(format_args!("{ticked} ticks in 100 ms"));
        // 100 at 1 kHz, far from 100 Hz or 10 kHz. Bounded generously:
        // QEMU's TCG skips the periods of a periodic timer it serves late
        // while the guest busy-waits (about 2/3 delivered), and a lock held
        // across a period merges ticks.
        kassert!((40..=150).contains(&ticked));
        Ok(())
    }
);

crate::ktest!(
    fn a_held_lock_holds_off_the_timer() {
        // While a lock is held (interrupts off) no tick arrives; the one
        // pending is delivered as soon as it is dropped.
        use crate::{kassert, kassert_eq};
        let lock = crate::sync::SpinLock::new(());
        let guard = lock.lock();
        let t0 = ticks();
        sleep(5_000_000);
        let during = ticks() - t0;
        drop(guard);
        sleep(2_000_000);
        kassert_eq!(during, 0);
        kassert!(ticks() > t0);
        Ok(())
    }
);

crate::ktest!(
    fn timer_ticks_are_not_traced() {
        // At 1 kHz they would flood the trace ring.
        use crate::{kassert, kassert_eq};
        let ring = crate::trace::ring();
        let (t0, w0) = (ticks(), ring.written());
        sleep(20_000_000);
        kassert!(ticks() > t0);
        kassert_eq!(ring.written(), w0);
        Ok(())
    }
);
