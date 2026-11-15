//! Device drivers: one table, [`DRIVERS`], and [`probe_all`], which offers
//! each PCI function to the first driver whose `matches` accepts it
//! (docs/ARCHITECTURE.md, section 7). Each driver's issue adds its row
//! (rng #97, the tablet #98, sound #99).

use alloc::string::ToString;
use core::sync::atomic::{AtomicBool, Ordering};

use log::{info, warn};

use crate::pci::{self, DeviceInfo, DeviceResources};
pub use crate::virtio::ProbeError;

/// A driver: which functions it takes, and how it brings one up. `probe`
/// gets the function's resources (and nothing else); an error is a
/// warning, recorded for `lspci`, and boot goes on.
pub struct Driver {
    pub name: &'static str,
    pub matches: fn(&DeviceInfo) -> bool,
    pub probe: fn(DeviceResources) -> Result<(), ProbeError>,
}

/// Every driver, in the order they are offered a function.
pub static DRIVERS: &[Driver] = &[];

static PROBED: AtomicBool = AtomicBool::new(false);

/// Offer every PCI function to [`DRIVERS`]: once, at boot, after
/// enumeration and once threads and timers run (a driver may wait or start
/// a thread). Panics if called twice.
pub fn probe_all() {
    assert!(!PROBED.swap(true, Ordering::Relaxed), "drivers::probe_all called twice");
    probe_with(DRIVERS);
}

/// What [`probe_all`] does, with any table (the ktests give their own).
/// Each matched function is claimed (`pci::claim`: a refusal is a warning
/// and `not probed: <reason>` in `lspci`), logged before its probe (`probing
/// <driver> on <addr>`, so a hang points at a device) and its outcome
/// recorded (`<driver>: bound`, or `<driver>: failed: <reason>` with a
/// warning that also gives the change in outstanding DMA runs, which a
/// probe failing partway leaks).
fn probe_with(drivers: &[Driver]) {
    let (mut bound, mut failed, mut refused) = (0, 0, 0);
    for d in pci::devices() {
        let Some(driver) = drivers.iter().find(|drv| (drv.matches)(d)) else { continue };
        let res = match pci::claim(d.addr, driver.name) {
            Ok(res) => res,
            Err(e) => {
                warn!(target: "drivers", "{}: not probed for {}: {e}", d.addr, driver.name);
                refused += 1;
                continue;
            }
        };
        info!(target: "drivers", "probing {} on {}", driver.name, d.addr);
        let runs = crate::virtio::outstanding_runs();
        let outcome = (driver.probe)(res);
        match &outcome {
            Ok(()) => bound += 1,
            Err(e) => {
                let now = crate::virtio::outstanding_runs();
                warn!(
                    target: "drivers",
                    "{}: {} probe failed: {e} (DMA runs outstanding: {runs} before, {now} after)",
                    d.addr,
                    driver.name
                );
                failed += 1;
            }
        }
        pci::probed(d.addr, driver.name, outcome.map_err(|e| e.to_string()));
    }
    let rows = drivers.len();
    let s = if rows == 1 { "" } else { "s" };
    info!(target: "drivers", "{bound} bound, {failed} failed, {refused} not probed ({rows} driver row{s})");
}

crate::ktest!(
    fn probing_records_each_outcome() {
        // The ktest machine's virtio 9p device: no driver takes it in M3.
        use crate::{kassert, kassert_eq};
        let is_9p = |d: &DeviceInfo| lumen_pci::names::virtio_type(d.vendor, d.device, d.subsystem.1) == Some(9);
        let addr = pci::devices().iter().find(|d| is_9p(d)).ok_or("no virtio 9p device")?.addr;
        kassert_eq!(pci::binding(addr), Some(pci::Binding::Free));
        // The first matching row is offered the function, and the second
        // never is. Its probe fails.
        let table = [
            Driver {
                name: "ktest-9p",
                matches: is_9p,
                probe: |_| Err(ProbeError::Wait(crate::virtio::WaitError::Timeout)),
            },
            Driver { name: "never", matches: is_9p, probe: |_| panic!("offered a device a row before it took") },
        ];
        probe_with(&table);
        kassert_eq!(
            pci::binding(addr),
            Some(pci::Binding::Failed("ktest-9p", "the device didn't answer in time".into()))
        );
        kassert_eq!(
            alloc::format!("{}", pci::binding(addr).ok_or("gone")?),
            "ktest-9p: failed: the device didn't answer in time"
        );
        // Claimed once: offered again (by any table), it isn't probed.
        let again = [Driver { name: "ktest-9p-again", matches: is_9p, probe: |_| panic!("probed twice") }];
        probe_with(&again);
        kassert!(matches!(pci::claim(addr, "ktest"), Err(pci::ClaimError::Taken(pci::Binding::Failed(..)))));
        Ok(())
    }
);
