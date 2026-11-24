//! Pid allocation: from [`FIRST_PID`] up to a maximum, then wrapping.

use crate::Errno;

/// Pid 1: never handed out by [`PidAllocator::alloc`]; it is for the process
/// the kernel names init ([`crate::ProcTree::spawn_init`]).
pub const INIT_PID: u32 = 1;

/// The lowest pid [`PidAllocator::alloc`] hands out.
pub const FIRST_PID: u32 = 2;

/// Hands out pids from [`FIRST_PID`] to its maximum, inclusive, each search
/// starting after the pid handed out last and wrapping back to
/// [`FIRST_PID`] once past the maximum, as Linux's `alloc_pid` cycles.
///
/// The allocator keeps no record of which pids are taken: the caller says,
/// through `alloc`'s `busy` predicate, which numbers are still in use.
/// [`crate::ProcTree`] counts a number in use while any process, live or
/// zombie, has it as its pid, its pgid or its sid (Linux keeps a pid busy
/// while a group or session uses it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PidAllocator {
    max: u32,
    /// The pid handed out last (the search starts after it).
    last: u32,
}

impl PidAllocator {
    /// An allocator handing out `FIRST_PID..=max_pid`. With `max_pid` below
    /// [`FIRST_PID`] every allocation fails with `EAGAIN`.
    ///
    /// # Panics
    ///
    /// If `max_pid` exceeds `i32::MAX`: a pid must be a positive `pid_t`.
    pub const fn new(max_pid: u32) -> Self {
        assert!(max_pid <= i32::MAX as u32, "a pid must fit in a positive pid_t");
        Self { max: max_pid, last: INIT_PID }
    }

    /// The largest pid this allocator hands out.
    pub const fn max_pid(&self) -> u32 {
        self.max
    }

    /// The first number after the last one handed out, wrapping past the
    /// maximum to [`FIRST_PID`], for which `busy` is false; or `EAGAIN` if
    /// `busy` holds for every number in `FIRST_PID..=max`.
    ///
    /// `busy` is called at most once per number, and only on busy numbers
    /// plus the one returned: at most (the count of busy numbers + 1) calls.
    /// Neither allocates nor frees. On `EAGAIN` the allocator is unchanged.
    pub fn alloc(&mut self, mut busy: impl FnMut(u32) -> bool) -> Result<u32, Errno> {
        if self.max < FIRST_PID {
            return Err(Errno::Again);
        }
        // `last < max <= i32::MAX`, so `last + 1` can't overflow.
        let start = if self.last >= self.max || self.last < FIRST_PID { FIRST_PID } else { self.last + 1 };
        let found = (start..=self.max).chain(FIRST_PID..start).find(|&n| !busy(n));
        match found {
            Some(pid) => {
                self.last = pid;
                Ok(pid)
            }
            None => Err(Errno::Again),
        }
    }
}
