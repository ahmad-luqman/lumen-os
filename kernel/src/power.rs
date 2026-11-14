//! Rebooting and powering off (the shell's `reboot` and `shutdown`).
//!
//! - Reboot tries a full reset through port 0xCF9, then the 8042's reset
//!   pulse, then a triple fault, waiting a little after each for the reset
//!   to take.
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
use alloc::string::String;
use log::{info, warn};

/// How long to wait for a reset method, or S5, to take effect.
const SETTLE_NS: u64 = 500_000_000;
/// How long the firmware gets to switch to ACPI mode.
const ACPI_MODE_NS: u64 = 3_000_000_000;

/// Reset the machine.
pub fn reboot() -> ! {
    info!(target: "power", "rebooting: reset through port 0xcf9");
    // For good: nothing below may be interrupted by a thread switch, and
    // nothing runs after a reset.
    irq::disable_for_good();
    reset::reset_control();
    crate::time::spin(SETTLE_NS);
    warn!(target: "power", "port 0xcf9 didn't reset the machine; pulsing the 8042's reset line");
    reset::keyboard_controller();
    crate::time::spin(SETTLE_NS);
    warn!(target: "power", "the 8042 didn't reset the machine; triple-faulting");
    reset::triple_fault()
}

/// What powering off takes, or why it can't be done.
pub fn power_off_plan() -> Result<&'static PowerOff, String> {
    let info = acpi::info().ok_or_else(|| String::from("no ACPI tables"))?;
    info.power_off.as_ref().map_err(String::clone)
}

/// Enter ACPI S5 as `plan` says. Returns (with interrupts as they were)
/// only if the machine is still on afterwards, saying why it might be.
pub fn power_off(plan: &PowerOff) -> String {
    let (fadt, s5) = (plan.fadt, plan.s5);
    info!(
        target: "power",
        "powering off: ACPI S5, SLP_TYP {}/{} into PM1a control {:#x}{}",
        s5.a,
        s5.b,
        fadt.pm1a_control,
        if fadt.pm1b_control.is_some() { " and PM1b" } else { "" }
    );
    let _off = irq::push_off();
    if !reset::acpi_mode(fadt.pm1a_control) {
        match fadt.smi_command {
            Some(port) if fadt.acpi_enable != 0 => {
                info!(target: "power", "switching to ACPI mode (SMI command {port:#x})");
                reset::enable_acpi(port, fadt.acpi_enable);
                let deadline = crate::time::now_ns() + ACPI_MODE_NS;
                while !reset::acpi_mode(fadt.pm1a_control) && crate::time::now_ns() < deadline {
                    core::hint::spin_loop();
                }
                if !reset::acpi_mode(fadt.pm1a_control) {
                    warn!(target: "power", "the firmware didn't switch to ACPI mode; trying S5 anyway");
                }
            }
            _ => warn!(target: "power", "not in ACPI mode and no way to ask for it; trying S5 anyway"),
        }
    }
    // The sleep type first, then SLP_EN, each into PM1a then PM1b (as
    // ACPICA does: some chipsets want the type in place before the enable).
    for enter in [false, true] {
        reset::pm1_sleep(fadt.pm1a_control, s5.a, enter);
        if let Some(port) = fadt.pm1b_control {
            reset::pm1_sleep(port, s5.b, enter);
        }
    }
    crate::time::spin(SETTLE_NS);
    String::from("the machine is still on after entering ACPI S5")
}
