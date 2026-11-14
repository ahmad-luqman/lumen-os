//! Rebooting and powering off (the shell's `reboot` and `shutdown`).
//!
//! - Reboot tries a full reset through port 0xCF9, then the 8042's reset
//!   pulse, then a triple fault, waiting half a second after each of the
//!   first two for the reset to take. `lumen.reboot=8042` or
//!   `lumen.reboot=triple` on the command line starts further down that
//!   list (for the tests, and for a machine whose 0xCF9 misbehaves).
//! - Power-off enters ACPI S5 (soft off): the sleep type values from the
//!   DSDT's `_S5_` package (found at boot, `acpi::PowerOff`) go into the
//!   PM1a (and PM1b) control registers, then SLP_EN. If the chipset isn't
//!   in ACPI mode yet (SCI_EN clear), the firmware is asked for it first
//!   (the FADT's SMI command).
//!
//! Both run with interrupts off on the calling CPU; the other CPUs aren't
//! stopped first (a reset or power-off takes them too).

use crate::acpi::{self, PowerOff};
use crate::arch::{irq, reset};
use alloc::format;
use alloc::string::String;
use log::{info, warn};
use lumen_acpi::{AcpiMode, Fadt};

/// How long to wait for a reset method, or S5, to take effect.
const SETTLE_NS: u64 = 500_000_000;
/// How long the firmware gets to switch to ACPI mode.
const ACPI_MODE_NS: u64 = 3_000_000_000;

/// The ways to reset, in the order they are tried.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Method {
    ResetControl,
    KeyboardController,
    TripleFault,
}

/// The first method to try: `lumen.reboot=`'s (`8042` or `triple`), else
/// port 0xCF9 (also for a value it doesn't know, with a warning).
fn first_method() -> Method {
    match crate::cmdline::value("lumen.reboot") {
        None | Some("cf9") => Method::ResetControl,
        Some("8042") => Method::KeyboardController,
        Some("triple") => Method::TripleFault,
        Some(other) => {
            warn!(target: "power", "lumen.reboot={other}: not cf9, 8042 or triple; starting with 0xcf9");
            Method::ResetControl
        }
    }
}

/// Reset the machine.
pub fn reboot() -> ! {
    let first = first_method();
    // For good: nothing below may be interrupted by a thread switch, and
    // nothing runs after a reset.
    irq::disable_for_good();
    if first <= Method::ResetControl {
        info!(target: "power", "rebooting: reset through port 0xcf9");
        reset::reset_control();
        crate::time::spin(SETTLE_NS);
        warn!(target: "power", "port 0xcf9 didn't reset the machine");
    }
    if first <= Method::KeyboardController {
        info!(target: "power", "rebooting: pulsing the 8042's reset line");
        let ready = reset::keyboard_controller();
        crate::time::spin(SETTLE_NS);
        if ready {
            warn!(target: "power", "the 8042 didn't reset the machine");
        } else {
            warn!(target: "power", "the 8042 didn't reset the machine (it never became ready for the command)");
        }
    }
    info!(target: "power", "rebooting: triple fault");
    reset::triple_fault()
}

/// What powering off takes, or why it can't be done.
pub fn power_off_plan() -> Result<&'static PowerOff, String> {
    let info = acpi::info().ok_or_else(|| String::from("ACPI wasn't usable at boot (see the boot log)"))?;
    info.power_off.as_ref().map_err(|e| format!("{e}"))
}

/// Enter ACPI S5 as `plan` says. Returns (interrupts as they were) only if
/// the machine is still on half a second later, with what it tried (also
/// logged as a warning).
pub fn power_off(plan: &PowerOff) -> String {
    let (fadt, s5) = (plan.fadt(), plan.s5());
    info!(
        target: "power",
        "powering off: ACPI S5, SLP_TYP {}/{} into PM1a control {:#x}{}",
        s5.a,
        s5.b,
        fadt.pm1a_control.port(),
        if fadt.pm1b_control.is_some() { " and PM1b" } else { "" }
    );
    let _off = irq::push_off();
    if !reset::acpi_mode(fadt.pm1a_control) {
        switch_to_acpi_mode(fadt);
    }
    reset::enter_sleep(fadt.pm1a_control, fadt.pm1b_control, s5);
    crate::time::spin(SETTLE_NS);
    let why = format!(
        "still on after ACPI S5 (SLP_TYP {}/{} into PM1a control {:#x}, SCI_EN {})",
        s5.a,
        s5.b,
        fadt.pm1a_control.port(),
        if reset::acpi_mode(fadt.pm1a_control) { "set" } else { "clear" }
    );
    warn!(target: "power", "{why}");
    why
}

/// The chipset isn't in ACPI mode (SCI_EN clear): ask the firmware for it
/// as the FADT says, waiting up to [`ACPI_MODE_NS`]; warn if that can't be
/// done or didn't work (S5 is tried anyway).
fn switch_to_acpi_mode(fadt: Fadt) {
    let enable = match fadt.acpi_mode {
        AcpiMode::Ask(enable) => enable,
        mode => {
            warn!(target: "power", "not in ACPI mode, and the FADT gives {mode}; trying S5 anyway");
            return;
        }
    };
    info!(target: "power", "switching to ACPI mode (SMI command {:#x})", enable.port());
    reset::enable_acpi(enable);
    let deadline = crate::time::now_ns() + ACPI_MODE_NS;
    while !reset::acpi_mode(fadt.pm1a_control) && crate::time::now_ns() < deadline {
        core::hint::spin_loop();
    }
    if !reset::acpi_mode(fadt.pm1a_control) {
        warn!(target: "power", "the firmware didn't switch to ACPI mode; trying S5 anyway");
    }
}
