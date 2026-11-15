//! What can be wrong with a function enumeration found, and the warning
//! the kernel logs for it (after the function's address).

use core::fmt;

use crate::{BarProblem, CapEnd};

/// One thing wrong with a function ([`DeviceInfo::problems`](crate::DeviceInfo::problems)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The capability list was cut off, or not read: `pci::claim` refuses
    /// the device (recorded as not probed).
    Caps(CapEnd),
    /// BAR `index` is implemented but the firmware assigned it no address.
    UnassignedBar { index: usize, io: bool, size: u64 },
    /// BAR `index` can't be used as it is.
    MalformedBar { index: usize, why: BarProblem },
    /// A bridge with secondary bus 0: not configured, so nothing behind it
    /// is reachable.
    BridgeUnconfigured,
    /// A bridge whose primary bus isn't its own, or whose secondary bus
    /// isn't above its own.
    BridgeMisnumbered { primary: u8, secondary: u8, subordinate: u8 },
    /// A bridge whose secondary bus is above its subordinate bus.
    BridgeBusesBackwards { secondary: u8, subordinate: u8 },
    /// A bridge whose buses aren't all in the configuration space being
    /// scanned: the devices behind it are invisible.
    BusesOutsideEcam { secondary: u8, subordinate: u8 },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Problem::Caps(end) => write!(f, "capability list incomplete: {end}; drivers won't be given it"),
            Problem::UnassignedBar { index, io, size } => {
                let space = if io { "I/O" } else { "memory" };
                write!(f, "BAR {index} ({space}, {size:#x} bytes) has no address: the firmware didn't assign it")
            }
            Problem::MalformedBar { index, why } => write!(f, "BAR {index} is unusable: {why}"),
            Problem::BridgeUnconfigured => {
                f.write_str("this bridge has no secondary bus (not configured): devices behind it aren't listed")
            }
            Problem::BridgeMisnumbered { primary, secondary, subordinate } => write!(
                f,
                "this bridge's bus numbers (primary {primary}, secondary {secondary}, subordinate {subordinate}) \
                 don't fit the bus it is on: devices behind it may not be listed"
            ),
            Problem::BridgeBusesBackwards { secondary, subordinate } => write!(
                f,
                "this bridge's secondary bus {secondary} is above its subordinate bus {subordinate}: \
                 devices behind it may not be listed"
            ),
            Problem::BusesOutsideEcam { secondary, subordinate } => write!(
                f,
                "the buses behind this bridge ({secondary}-{subordinate}) aren't all in mapped configuration space: \
                 devices there aren't listed"
            ),
        }
    }
}
