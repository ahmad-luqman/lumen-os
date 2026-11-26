//! Sessions and process groups: `setsid`, `setpgid`, `getpgid`, `getsid`,
//! and the checks of `tcsetpgrp`.

use crate::tree::Proc;
use crate::{Errno, Error, ProcTree, Role};

/// The caller's controlling terminal, when the terminal a `tcsetpgrp` is on
/// is it, as the kernel's terminal side knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controlling {
    /// The session it is the controlling terminal of (Linux's
    /// `tty->ctrl.session`), if any.
    pub session: Option<u32>,
    /// Its foreground process group (`tty->ctrl.pgrp`), if any.
    pub foreground: Option<u32>,
}

/// The caller's disposition of `SIGTTOU`, which decides what happens when a
/// background process calls `tcsetpgrp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sigttou {
    /// Neither blocked nor ignored (default action or a handler): it would
    /// be delivered.
    Delivered,
    /// Blocked, or ignored (`SIG_IGN`).
    BlockedOrIgnored,
}

/// What a `tcsetpgrp` that passed its checks does.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tcsetpgrp {
    /// Make this number (a process group's id, or the pid of a process
    /// leading none) the terminal's foreground group; return 0.
    Set(u32),
    /// The caller is in the background: send `SIGTTOU` to its process group
    /// `pgrp` and return `-ERESTARTSYS`. The call restarts after the stop
    /// ends, or after a handler installed with `SA_RESTART`; after a handler
    /// without it, the call fails with `EINTR`.
    Sigttou {
        /// The caller's process group.
        pgrp: u32,
    },
}

impl ProcTree {
    /// `setsid` by `caller`: a new session and group, both its pid, which it
    /// returns. `EPERM` if some process (live or zombie) has `caller`'s pid
    /// as its pgid: it leads a group (as every session leader does), or
    /// left one others are still in. [`crate::Bug::NotLive`] if `caller`
    /// isn't live. Allocates infallibly (the indexes).
    pub fn setsid(&mut self, caller: u32) -> Result<u32, Error> {
        self.live(caller, Role::Caller)?;
        if self.pgrp_exists(caller) {
            return Err(Errno::Perm.into());
        }
        self.set_pgid(caller, caller);
        self.set_sid(caller, caller);
        Ok(caller)
    }

    /// `setpgid(pid, pgid)` by `caller`, with Linux's checks in Linux's order:
    /// 1. `pid` 0 means `caller`, then `pgid` 0 means `pid`; `EINVAL` if
    ///    `pgid` is negative;
    /// 2. `ESRCH` if `pid` names no process (or is negative);
    /// 3. for a child of `caller`: `EPERM` if it is in another session, then
    ///    `EACCES` if it has exec'd; for anyone else but `caller`: `ESRCH`;
    /// 4. `EPERM` if the target leads a session;
    /// 5. unless `pgid` is the target's own pid (a new group it leads),
    ///    `EPERM` if no process has `pgid` as its group or that group is in
    ///    another session than `caller`'s.
    ///
    /// A zombie child that never exec'd can be moved, as in Linux.
    /// [`crate::Bug::NotLive`] if `caller` isn't live. Allocates infallibly
    /// (the index).
    pub fn setpgid(&mut self, caller: u32, pid: i32, pgid: i32) -> Result<(), Error> {
        let c_sid = self.live(caller, Role::Caller)?.sid;
        let pid = if pid == 0 { caller as i32 } else { pid };
        let pgid = if pgid == 0 { pid } else { pgid };
        if pgid < 0 {
            return Err(Errno::Inval.into());
        }
        let (pid, t) = self.by_pid(caller, pid)?;
        if t.parent == Some(caller) {
            if t.sid != c_sid {
                return Err(Errno::Perm.into());
            }
            if t.execed {
                return Err(Errno::Acces.into());
            }
        } else if pid != caller {
            return Err(Errno::Srch.into());
        }
        if t.sid == pid {
            return Err(Errno::Perm.into());
        }
        let pgid = pgid as u32;
        if pgid != pid && self.pgrp_sid(pgid) != Some(c_sid) {
            return Err(Errno::Perm.into());
        }
        self.set_pgid(pid, pgid);
        Ok(())
    }

    /// `getpgid(pid)` by `caller` (`pid` 0: the caller). `ESRCH` if `pid`
    /// names no process, live or zombie, or is negative.
    /// [`crate::Bug::NotLive`] if `caller` isn't live.
    pub fn getpgid(&self, caller: u32, pid: i32) -> Result<u32, Error> {
        self.live(caller, Role::Caller)?;
        Ok(self.by_pid(caller, pid)?.1.pgid)
    }

    /// `getsid(pid)` by `caller` (`pid` 0: the caller). `ESRCH` if `pid`
    /// names no process, live or zombie, or is negative.
    /// [`crate::Bug::NotLive`] if `caller` isn't live.
    pub fn getsid(&self, caller: u32, pid: i32) -> Result<u32, Error> {
        self.live(caller, Role::Caller)?;
        Ok(self.by_pid(caller, pid)?.1.sid)
    }

    /// `tcsetpgrp(fd, pgrp)` by `caller`, with the checks of Linux's
    /// `tiocspgrp` in its order. `controlling` is the caller's controlling
    /// terminal when the fd's terminal is it, `None` otherwise.
    /// 1. on its controlling terminal, a caller outside the foreground group
    ///    (when there is one) is in the background: with `SIGTTOU` blocked or
    ///    ignored it goes on; otherwise `ENOTTY` if its group is orphaned,
    ///    else [`Tcsetpgrp::Sigttou`];
    /// 2. `EINVAL` if `pgrp` is negative;
    /// 3. `ENOTTY` unless the terminal is the caller's controlling terminal
    ///    and belongs to the caller's session;
    /// 4. `ESRCH` if no process uses `pgrp` as a pid, pgid or sid;
    /// 5. `EPERM` unless the group `pgrp` (or, without one, the process
    ///    `pgrp`) is in the caller's session.
    ///
    /// [`crate::Bug::NotLive`] if `caller` isn't live. Neither allocates nor
    /// frees.
    pub fn tcsetpgrp(
        &self,
        caller: u32,
        controlling: Option<Controlling>,
        pgrp: i32,
        sigttou: Sigttou,
    ) -> Result<Tcsetpgrp, Error> {
        if let Some(pgrp) = self.tcsetpgrp_job_control(caller, controlling, sigttou)? {
            return Ok(Tcsetpgrp::Sigttou { pgrp });
        }
        let c_sid = self.live(caller, Role::Caller)?.sid;
        if pgrp < 0 {
            return Err(Errno::Inval.into());
        }
        if controlling.and_then(|t| t.session) != Some(c_sid) {
            return Err(Errno::NoTty.into());
        }
        let n = pgrp as u32;
        if !self.is_busy(n) {
            return Err(Errno::Srch.into());
        }
        let session = self.pgrp_sid(n).or(self.sid(n));
        if session != Some(c_sid) {
            return Err(Errno::Perm.into());
        }
        Ok(Tcsetpgrp::Set(n))
    }

    /// Check `TIOCSPGRP` job control before its argument is copied from userspace.
    /// Returns the group to signal, `ENOTTY` for an orphaned background group,
    /// or `None` to proceed. The full check must run again after the copy.
    /// Neither allocates nor frees. [`crate::Bug::NotLive`] for a dead caller.
    pub fn tcsetpgrp_job_control(
        &self,
        caller: u32,
        controlling: Option<Controlling>,
        sigttou: Sigttou,
    ) -> Result<Option<u32>, Error> {
        let c_pgid = self.live(caller, Role::Caller)?.pgid;
        if let Some(Controlling { foreground: Some(fg), .. }) = controlling
            && fg != c_pgid
            && sigttou == Sigttou::Delivered
        {
            return if self.is_orphaned_pgrp(c_pgid) { Err(Errno::NoTty.into()) } else { Ok(Some(c_pgid)) };
        }
        Ok(None)
    }

    /// The process `pid` names (0: `caller`), live or zombie, with its pid;
    /// `ESRCH` for a negative `pid` or one naming no process.
    fn by_pid(&self, caller: u32, pid: i32) -> Result<(u32, &Proc), Errno> {
        let pid = if pid == 0 { caller } else { u32::try_from(pid).map_err(|_| Errno::Srch)? };
        self.procs.get(&pid).map(|p| (pid, p)).ok_or(Errno::Srch)
    }
}
