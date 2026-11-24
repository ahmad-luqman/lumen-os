//! Sessions and process groups: `setsid`, `setpgid`, `getpgid`, `getsid`,
//! and the checks of `tcsetpgrp`.

use crate::{Errno, ProcTree};

/// The terminal a `tcsetpgrp` is on, as the kernel's terminal side knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terminal {
    /// Whether it is the caller's controlling terminal.
    pub is_controlling: bool,
    /// The session it is the controlling terminal of, if any.
    pub session: Option<u32>,
    /// Its foreground process group, if any.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tcsetpgrp {
    /// Make this process group the terminal's foreground group; return 0.
    Set(u32),
    /// The caller is in the background: send `SIGTTOU` to its process group
    /// `pgrp` and restart the call once the signal is handled
    /// (`-ERESTARTSYS`).
    Sigttou {
        /// The caller's process group.
        pgrp: u32,
    },
}

impl ProcTree {
    /// `setsid` by `caller`: a new session and group, both its pid, which it
    /// returns. `EPERM` if `caller` leads a session already, or some process
    /// (live or zombie) has its pid as its pgid: it leads a group, or left
    /// one others are still in. `ESRCH` if `caller` isn't live. Can allocate.
    pub fn setsid(&mut self, caller: u32) -> Result<u32, Errno> {
        let c = self.live(caller)?;
        if c.sid == caller || self.pgrp_exists(caller) {
            return Err(Errno::Perm);
        }
        self.set_ids(caller, caller, caller);
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
    /// A zombie child that never exec'd can be moved, as in Linux. `ESRCH`
    /// if `caller` isn't live. Can allocate.
    pub fn setpgid(&mut self, caller: u32, pid: i32, pgid: i32) -> Result<(), Errno> {
        let c_sid = self.live(caller)?.sid;
        let pid = if pid == 0 { caller as i32 } else { pid };
        let pgid = if pgid == 0 { pid } else { pgid };
        if pgid < 0 {
            return Err(Errno::Inval);
        }
        let Some(target) = u32::try_from(pid).ok().and_then(|p| self.proc(p).map(|t| (p, t))) else {
            return Err(Errno::Srch);
        };
        let (pid, t) = target;
        if t.parent == Some(caller) {
            if t.sid != c_sid {
                return Err(Errno::Perm);
            }
            if t.execed {
                return Err(Errno::Acces);
            }
        } else if pid != caller {
            return Err(Errno::Srch);
        }
        if t.sid == pid {
            return Err(Errno::Perm);
        }
        let t_sid = t.sid;
        let pgid = pgid as u32;
        if pgid != pid {
            let group_sid = self.pgrp_members(pgid).next().and_then(|m| self.sid(m));
            if group_sid != Some(c_sid) {
                return Err(Errno::Perm);
            }
        }
        self.set_ids(pid, pgid, t_sid);
        Ok(())
    }

    /// `getpgid(pid)` by `caller` (`pid` 0: the caller). `ESRCH` if `pid`
    /// names no process, live or zombie, or is negative.
    pub fn getpgid(&self, caller: u32, pid: i32) -> Result<u32, Errno> {
        self.lookup(caller, pid).map(|p| p.0)
    }

    /// `getsid(pid)` by `caller` (`pid` 0: the caller). `ESRCH` if `pid`
    /// names no process, live or zombie, or is negative.
    pub fn getsid(&self, caller: u32, pid: i32) -> Result<u32, Errno> {
        self.lookup(caller, pid).map(|p| p.1)
    }

    /// `tcsetpgrp(fd, pgrp)` by `caller` on `tty`, with the checks of Linux's
    /// `tiocspgrp` in its order:
    /// 1. on its controlling terminal, a caller outside the foreground group
    ///    (when there is one) is in the background: with `SIGTTOU` blocked or
    ///    ignored it goes on; otherwise `ENOTTY` if its group is orphaned,
    ///    else [`Tcsetpgrp::Sigttou`];
    /// 2. `EINVAL` if `pgrp` is negative;
    /// 3. `ENOTTY` unless `tty` is the caller's controlling terminal and
    ///    belongs to the caller's session;
    /// 4. `ESRCH` if no process uses `pgrp` as a pid, pgid or sid;
    /// 5. `EPERM` unless the group `pgrp` (or, without one, the process
    ///    `pgrp`) is in the caller's session.
    ///
    /// `ESRCH` if `caller` isn't live. Neither allocates nor frees.
    pub fn tcsetpgrp(&self, caller: u32, tty: Terminal, pgrp: i32, sigttou: Sigttou) -> Result<Tcsetpgrp, Errno> {
        let c = self.live(caller)?;
        let (c_pgid, c_sid) = (c.pgid, c.sid);
        if tty.is_controlling
            && let Some(fg) = tty.foreground
            && fg != c_pgid
            && sigttou == Sigttou::Delivered
        {
            return if self.is_orphaned_pgrp(c_pgid) {
                Err(Errno::NoTty)
            } else {
                Ok(Tcsetpgrp::Sigttou { pgrp: c_pgid })
            };
        }
        if pgrp < 0 {
            return Err(Errno::Inval);
        }
        if !tty.is_controlling || tty.session != Some(c_sid) {
            return Err(Errno::NoTty);
        }
        let n = pgrp as u32;
        if !self.is_busy(n) {
            return Err(Errno::Srch);
        }
        let session = self.pgrp_members(n).next().or(self.contains(n).then_some(n)).and_then(|m| self.sid(m));
        if session != Some(c_sid) {
            return Err(Errno::Perm);
        }
        Ok(Tcsetpgrp::Set(n))
    }

    /// The `(pgid, sid)` of `pid` (0: `caller`), live or zombie.
    fn lookup(&self, caller: u32, pid: i32) -> Result<(u32, u32), Errno> {
        let pid = if pid == 0 { caller } else { u32::try_from(pid).map_err(|_| Errno::Srch)? };
        self.proc(pid).map(|p| (p.pgid, p.sid)).ok_or(Errno::Srch)
    }
}
