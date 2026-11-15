//! Device drivers: one table, [`DRIVERS`], and [`probe_all`], which offers
//! each PCI function to the first driver whose `matches` accepts it
//! (docs/ARCHITECTURE.md, section 7). Each driver's issue adds its row
//! (rng #97, the tablet #98, sound #99).

use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

use log::{info, warn};

use crate::pci::{self, DeviceInfo, DeviceResources, Probed};
pub use crate::virtio::ProbeError;

/// A driver: which functions it takes, and how it brings one up. `probe`
/// gets the function's resources (and nothing else); an error is a
/// warning, recorded for `lspci`, and boot goes on. A virtio driver calls
/// `virtio::transport`, then the crate's `new()`, then the transport's
/// `StatusHandle::check_ready`; it returns [`ProbeError::NotMine`] for a
/// device that turns out not to be its own.
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
/// Each function is offered to the matching rows in order. The row's claim
/// (`pci::claim`) can be refused (a warning; `lspci` shows `<driver>: not
/// probed: <reason>`, or keeps the binding of a function claimed before).
/// Before each probe it logs `probing <driver> on <addr>`, so a hang points
/// at a device; runs the probe hands out are recorded as that function's.
/// The outcome is recorded: `<driver>: bound`; `<driver>: failed: <reason>`
/// (a warning, saying how many DMA runs the failed probe left behind:
/// buffers still posted when its queues were dropped); or declined
/// ([`ProbeError::NotMine`]: the function is freed, its command register
/// restored, and the next matching row is offered it).
fn probe_with(drivers: &[Driver]) {
    let (mut bound, mut failed, mut refused) = (0, 0, 0);
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
            crate::virtio::set_probe_owner(Some(d.addr));
            let outcome = (driver.probe)(res);
            crate::virtio::set_probe_owner(None);
            let leaked = crate::virtio::outstanding_runs_of(d.addr);
            let left =
                if leaked == 0 { String::new() } else { alloc::format!(" ({leaked} DMA runs left outstanding)") };
            match outcome {
                Ok(()) => {
                    bound += 1;
                    pci::probed(d.addr, driver.name, Probed::Bound);
                    break;
                }
                Err(ProbeError::NotMine) => {
                    info!(target: "drivers", "{}: {} declined it{left}", d.addr, driver.name);
                    pci::probed(d.addr, driver.name, Probed::Declined);
                }
                Err(e) => {
                    warn!(target: "drivers", "{}: {} probe failed: {e}{left}", d.addr, driver.name);
                    failed += 1;
                    pci::probed(d.addr, driver.name, Probed::Failed(alloc::format!("{e}{left}")));
                    break;
                }
            }
        }
    }
    let rows = drivers.len();
    let s = if rows == 1 { "" } else { "s" };
    info!(target: "drivers", "{bound} bound, {failed} failed, {refused} not probed ({rows} driver row{s})");
}

crate::ktest!(
    fn probing_records_each_outcome() {
        // The ktest machine's virtio 9p and balloon devices: no driver
        // takes them in M3.
        use crate::pci::{Binding, ClaimError, ktest_command};
        use crate::{kassert, kassert_eq};
        use lumen_pci::command::{BUS_MASTER, MEMORY_SPACE};
        let is_9p = |d: &DeviceInfo| d.virtio_type() == Some(9);
        let is_balloon = |d: &DeviceInfo| d.virtio_type() == Some(5);
        let find =
            |is: fn(&DeviceInfo) -> bool, what| pci::devices().iter().find(|d| is(d)).map(|d| d.addr).ok_or(what);
        let (p9, balloon) = (find(is_9p, "no virtio 9p device")?, find(is_balloon, "no virtio balloon device")?);
        kassert_eq!(pci::binding(p9), Some(Binding::Free));

        // A row that declines: the function is free again, its command
        // register as before the claim.
        let before = ktest_command(p9, None);
        probe_with(&[Driver { name: "ktest-decline", matches: is_9p, probe: |_| Err(ProbeError::NotMine) }]);
        kassert_eq!(pci::binding(p9), Some(Binding::Free));
        kassert_eq!(ktest_command(p9, None), before);

        // Declined by the first row, offered to the second, whose probe
        // fails; the third is never offered it. A row that binds.
        let table = [
            Driver { name: "ktest-decline", matches: is_9p, probe: |_| Err(ProbeError::NotMine) },
            Driver {
                name: "ktest-9p",
                matches: is_9p,
                probe: |_| Err(ProbeError::Wait(crate::virtio::WaitError::Timeout { waited_ns: 0, status: 0 })),
            },
            Driver { name: "never", matches: is_9p, probe: |_| panic!("offered a device a row before it took") },
            Driver { name: "ktest-balloon", matches: is_balloon, probe: |_| Ok(()) },
        ];
        probe_with(&table);
        let why = "the device didn't answer in 0 ms (status 0x00)";
        kassert_eq!(pci::binding(p9), Some(Binding::Failed("ktest-9p", why.into())));
        kassert_eq!(alloc::format!("{}", pci::binding(p9).ok_or("gone")?), alloc::format!("ktest-9p: failed: {why}"));
        // A failed device's bus mastering is off, its memory space still on.
        kassert_eq!(ktest_command(p9, None) & (MEMORY_SPACE | BUS_MASTER), MEMORY_SPACE);
        kassert_eq!(pci::binding(balloon), Some(Binding::Bound("ktest-balloon")));
        kassert_eq!(ktest_command(balloon, None) & (MEMORY_SPACE | BUS_MASTER), MEMORY_SPACE | BUS_MASTER);

        // Claimed once: offered again (by any table), it isn't probed.
        let again = [Driver { name: "ktest-9p-again", matches: is_9p, probe: |_| panic!("probed twice") }];
        probe_with(&again);
        kassert!(matches!(pci::claim(p9, "ktest"), Err(ClaimError::Taken(Binding::Failed(..)))));
        Ok(())
    }
);
