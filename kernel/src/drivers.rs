//! Device drivers: one table, [`DRIVERS`], and [`probe_all`], which offers
//! each PCI function to the driver rows whose `matches` accepts it, in
//! order, until one doesn't decline it (docs/ARCHITECTURE.md, section 7). Each driver's issue adds its row
//! (rng #97, the tablet #98, sound #99).

use alloc::string::String;
#[cfg(feature = "ktest")]
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use log::{info, warn};

use crate::pci::{self, Binding, DeviceInfo, DeviceResources, Probed};
pub use crate::virtio::ProbeError;

/// A driver: which functions it takes, and how it brings one up. `probe`
/// gets the function's resources (and nothing else); an error is a
/// warning, recorded for `lspci`, and boot goes on. A virtio driver calls
/// `virtio::transport` from the probe itself (runs are charged to the
/// probe attempt, and `probe_all` checks that transport's device is ready
/// when the probe returns `Ok`), then the crate's `new()`. It may return
/// [`ProbeError::NotMine`] for a device that turns out not to be its own,
/// but only before the crate's `new()` (reading the device configuration
/// through the transport): after it, the crate has posted buffers it never
/// takes back. A probe's side effects (a thread started, state published
/// for the rest of the kernel) come after its last fallible check
/// (`StatusHandle::check_ready` included), so a failed probe leaves none
/// behind.
pub struct Driver {
    pub name: &'static str,
    pub matches: fn(&DeviceInfo) -> bool,
    pub probe: fn(DeviceResources) -> Result<(), ProbeError>,
}

/// Every driver, in the order they are offered a function.
pub static DRIVERS: &[Driver] = &[crate::mouse::DRIVER];

static PROBED: AtomicBool = AtomicBool::new(false);

/// Offer every PCI function to [`DRIVERS`]: once, at boot, after
/// enumeration and once threads and timers run (a driver may wait or start
/// a thread). Panics if called twice.
pub fn probe_all() {
    assert!(!PROBED.swap(true, Ordering::Relaxed), "drivers::probe_all called twice");
    probe_with(DRIVERS);
}

/// What [`probe_all`] does, with any table (the ktests give their own).
/// Each function is offered to the matching rows in order, until one
/// doesn't decline it. The row's claim (`pci::claim`) can be refused (a
/// warning; `lspci` shows `<driver>: not probed: <reason>`, or keeps the
/// binding of a function claimed before). Before each probe it logs
/// `probing <driver> on <addr>`, so a hang points at a device; runs handed
/// out during the probe are recorded as that attempt's
/// (`virtio::begin_probe`). The outcome is recorded:
/// - `Ok`: if the probe brought up a virtio transport, its device must be
///   ready (`StatusHandle::check_ready`: the crate never reads
///   `FEATURES_OK` back), else the probe failed; `<driver>: bound`.
/// - [`ProbeError::NotMine`]: declined; the function is freed, its command
///   register restored, and the next matching row is offered it (refused,
///   recorded as failed, if the probe kept its `DeviceResources`).
/// - any other error: `<driver>: failed: <reason>`, a warning.
///
/// A failed or declined probe that left DMA runs outstanding (buffers still
/// posted when its queues were dropped) says how many, in a warning.
fn probe_with(drivers: &[Driver]) {
    let (mut bound, mut failed, mut declined, mut refused) = (0, 0, 0, 0);
    for d in pci::devices() {
        for driver in drivers.iter().filter(|drv| (drv.matches)(d)) {
            let res = match pci::claim(d.addr, driver.name) {
                Ok(res) => res,
                Err(e) => {
                    warn!(target: "drivers", "{}: not probed for {}: {e}", d.addr, driver.name);
                    refused += 1;
                    break;
                }
            };
            info!(target: "drivers", "probing {} on {}", driver.name, d.addr);
            let attempt = crate::virtio::begin_probe(d.addr);
            let mut outcome = (driver.probe)(res);
            let status = crate::virtio::end_probe();
            if outcome.is_ok()
                && let Some(status) = status
            {
                outcome = status.check_ready();
            }
            let leaked = crate::virtio::outstanding_runs_of(attempt);
            let left =
                if leaked == 0 { String::new() } else { alloc::format!(" ({leaked} DMA runs left outstanding)") };
            let probed = match outcome {
                Ok(()) => pci::probed(d.addr, driver.name, Probed::Bound),
                Err(ProbeError::NotMine) => {
                    if leaked == 0 {
                        info!(target: "drivers", "{}: {} declined it", d.addr, driver.name);
                    } else {
                        warn!(target: "drivers", "{}: {} declined it{left}", d.addr, driver.name);
                    }
                    pci::probed(d.addr, driver.name, Probed::Declined)
                }
                Err(e) => {
                    warn!(target: "drivers", "{}: {} probe failed: {e}{left}", d.addr, driver.name);
                    pci::probed(d.addr, driver.name, Probed::Failed(alloc::format!("{e}{left}")))
                }
            };
            match probed {
                Binding::Free { .. } => declined += 1,
                Binding::Bound(_) => {
                    bound += 1;
                    break;
                }
                _ => {
                    failed += 1;
                    break;
                }
            }
        }
    }
    let rows = drivers.len();
    let s = if rows == 1 { "" } else { "s" };
    info!(
        target: "drivers",
        "{bound} bound, {failed} failed, {declined} declined, {refused} not probed ({rows} driver row{s})"
    );
}

crate::ktest!(
    fn probing_records_each_outcome() {
        // The ktest machine's virtio 9p, balloon, console and SCSI devices:
        // no driver takes them in M3.
        use lumen_pci::command::{BUS_MASTER, MEMORY_SPACE};
        use virtio_drivers::transport::{DeviceStatus, Transport};
        use virtio_drivers::{BufferDirection, Hal};

        use crate::pci::{ClaimError, ktest_command};
        use crate::sync::SpinLock;
        use crate::virtio::{self, VirtioHal, VirtioTransport, WaitError};
        use crate::{kassert, kassert_eq};
        let is_9p: fn(&DeviceInfo) -> bool = |d| d.virtio_type() == Some(9);
        let is_balloon: fn(&DeviceInfo) -> bool = |d| d.virtio_type() == Some(5);
        let is_console: fn(&DeviceInfo) -> bool = |d| d.virtio_type() == Some(3);
        let is_scsi: fn(&DeviceInfo) -> bool = |d| d.virtio_type() == Some(8);
        let addr = |kind| virtio::ktest_device(kind).map(|d| d.addr);
        let (p9, balloon, console, scsi) = (addr(9)?, addr(5)?, addr(3)?, addr(8)?);
        kassert_eq!(pci::binding(p9), Some(Binding::Free { declined_by: None }));

        // A row that declines: the function is free again (declined, which
        // `lspci` says), its command register as before the claim.
        let before = ktest_command(p9, None);
        probe_with(&[Driver { name: "ktest-decline", matches: is_9p, probe: |_| Err(ProbeError::NotMine) }]);
        kassert_eq!(pci::binding(p9), Some(Binding::Free { declined_by: Some("ktest-decline") }));
        kassert_eq!(crate::shell::sysinfo::driver_text(p9), "no driver (ktest-decline declined it)");
        kassert_eq!(ktest_command(p9, None), before);

        // The runs each probe hands out, and the transport and resources
        // the probes keep.
        static RUNS: SpinLock<Vec<u64>> = SpinLock::new(Vec::new());
        static KEPT: SpinLock<Option<VirtioTransport>> = SpinLock::new(None);
        static KEPT_RES: SpinLock<Option<DeviceResources>> = SpinLock::new(None);
        fn leave_a_run() {
            let (p, _) = VirtioHal::dma_alloc(1, BufferDirection::DriverToDevice);
            RUNS.lock().push(p);
        }
        // A run outside any probe isn't charged to one.
        let (unowned, unowned_v) = VirtioHal::dma_alloc(1, BufferDirection::DriverToDevice);
        let table = [
            // Declines 9p, leaving a run (not charged to the next row)...
            Driver {
                name: "ktest-decline",
                matches: is_9p,
                probe: |_| {
                    leave_a_run();
                    Err(ProbeError::NotMine)
                },
            },
            // ... which fails, leaving one of its own; the next row is
            // never offered it.
            Driver {
                name: "ktest-9p",
                matches: is_9p,
                probe: |_| {
                    leave_a_run();
                    Err(ProbeError::Wait(WaitError::Timeout { waited_ns: 0, status: 0 }))
                },
            },
            Driver { name: "never", matches: is_9p, probe: |_| panic!("offered a device a row before it took") },
            // Brings up a transport and returns Ok, but the device isn't
            // ready (dropping the transport reset it): failed.
            Driver {
                name: "ktest-unready",
                matches: is_balloon,
                probe: |res| {
                    virtio::transport(res)?;
                    Ok(())
                },
            },
            // Keeps a running transport: bound.
            Driver {
                name: "ktest-console",
                matches: is_console,
                probe: |res| {
                    let mut t = virtio::transport(res)?;
                    t.set_status(
                        DeviceStatus::ACKNOWLEDGE
                            | DeviceStatus::DRIVER
                            | DeviceStatus::FEATURES_OK
                            | DeviceStatus::DRIVER_OK,
                    );
                    *KEPT.lock() = Some(t);
                    Ok(())
                },
            },
            // Declines but keeps its resources: refused, recorded failed.
            Driver {
                name: "ktest-keeper",
                matches: is_scsi,
                probe: |res| {
                    *KEPT_RES.lock() = Some(res);
                    Err(ProbeError::NotMine)
                },
            },
            Driver { name: "never", matches: is_scsi, probe: |_| panic!("offered a device a row before it took") },
        ];
        probe_with(&table);
        let why = "the device didn't answer in 0 ms (status 0x00) (1 DMA runs left outstanding)";
        kassert_eq!(pci::binding(p9), Some(Binding::Failed("ktest-9p", why.into())));
        kassert_eq!(alloc::format!("{}", pci::binding(p9).ok_or("gone")?), alloc::format!("ktest-9p: failed: {why}"));
        kassert_eq!(virtio::outstanding_runs_of_function(p9), 2);
        // A failed device's bus mastering is off, its memory space still on.
        kassert_eq!(ktest_command(p9, None) & (MEMORY_SPACE | BUS_MASTER), MEMORY_SPACE);
        kassert_eq!(
            pci::binding(balloon),
            Some(Binding::Failed(
                "ktest-unready",
                "the device status reads 0x00 after setup: FEATURES_OK clear: the device refused the negotiated \
                 features"
                    .into()
            ))
        );
        kassert_eq!(pci::binding(console), Some(Binding::Bound("ktest-console")));
        kassert_eq!(ktest_command(console, None) & (MEMORY_SPACE | BUS_MASTER), MEMORY_SPACE | BUS_MASTER);
        kassert_eq!(
            pci::binding(scsi),
            Some(Binding::Failed("ktest-keeper", "declined the device but kept its resources".into()))
        );
        // A run handed out after the probes isn't any function's.
        let (after, after_v) = VirtioHal::dma_alloc(1, BufferDirection::DriverToDevice);
        kassert_eq!(virtio::outstanding_runs_of_function(p9), 2);
        // SAFETY: runs this test allocated, unused.
        unsafe {
            kassert_eq!(VirtioHal::dma_dealloc(after, after_v, 1), 0);
            kassert_eq!(VirtioHal::dma_dealloc(unowned, unowned_v, 1), 0);
            for p in RUNS.lock().drain(..) {
                let v = core::ptr::NonNull::new(
                    crate::mm::phys_to_virt(lumen_mm::PhysAddr::new(p).ok_or("phys")?).as_mut_ptr::<u8>(),
                )
                .ok_or("null")?;
                kassert_eq!(VirtioHal::dma_dealloc(p, v, 1), 0);
            }
        }
        kassert_eq!(virtio::outstanding_runs_of_function(p9), 0);

        // Claimed once: offered again (by any table), it isn't probed.
        let again = [Driver { name: "ktest-9p-again", matches: is_9p, probe: |_| panic!("probed twice") }];
        probe_with(&again);
        kassert!(matches!(pci::claim(p9, "ktest"), Err(ClaimError::Taken(Binding::Failed(..)))));
        // The bound transport and kept resources stay (their devices are
        // the ktests').
        kassert!(KEPT.lock().is_some() && KEPT_RES.lock().is_some());
        Ok(())
    }
);
