//! The kernel's per-process `rusage` token, owned ([`RusageSlot`]) or lent
//! ([`SlotRef`]).

/// The kernel's opaque token for a process's resource usage. Moved into the
/// tree when the process is created ([`crate::ProcTree::spawn_init`],
/// [`crate::ProcTree::spawn_root`], [`crate::ProcTree::fork`]) and moved out
/// once, when it leaves the tree ([`crate::Report::Reaped`],
/// [`crate::ExitAction`]'s released slots): then the kernel frees it. Not
/// `Copy` or `Clone`, so it can't be freed twice by mistake.
#[derive(Debug, PartialEq, Eq)]
pub struct RusageSlot(u64);

impl RusageSlot {
    /// The token `value` (what the kernel's table uses).
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Its value, for the kernel's table.
    pub const fn get(&self) -> u64 {
        self.0
    }

    pub(crate) const fn into_raw(self) -> u64 {
        self.0
    }
}

/// A process's token, only lent ([`crate::Report::Zombie`], a `WNOWAIT`
/// report): the kernel may read the usage it names, but must neither free
/// it nor add it to the parent's totals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotRef(pub(crate) u64);

impl SlotRef {
    /// Its value, for the kernel's table.
    pub const fn get(self) -> u64 {
        self.0
    }
}
