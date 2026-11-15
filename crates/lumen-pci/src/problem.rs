//! What can be wrong with a function enumeration found, and the warning
//! the kernel logs for it (after the function's address).

use core::fmt;

use crate::{BarProblem, CapEnd};

/// One thing wrong with a function ([`DeviceInfo::problems`](crate::DeviceInfo::problems)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The capability list was cut off, or not read: #95's `probe_all`
    /// skips the device.
    Caps(CapEnd),
    /// BAR `index` is implemented but the firmware assigned it no address.
    UnassignedBar { index: usize, io: bool, size: u64 },
    /// BAR `index` can't be used as it is.
    MalformedBar { index: usize, why: BarProblem },
    /// A bridge whose buses aren't all in the configuration space being
    /// scanned (or are numbered backwards): the devices behind it are
    /// invisible.
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
            Problem::BusesOutsideEcam { secondary, subordinate } => write!(
                f,
                "the buses behind this bridge ({secondary}-{subordinate}) aren't all in mapped configuration space: \
                 devices there aren't listed"
            ),
        }
    }
}
