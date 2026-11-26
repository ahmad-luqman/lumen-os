//! PTY line disciplines, readiness and controlling-terminal operations.
//!
//! The sleep lock is Lock 9a: never held across user copies or other VFS
//! locks. Process-tree and signal locks may nest beneath it. Sequence changes
//! are published under this lock before waking readers, writers and pollers.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use lumen_proc::{Controlling, PidRef, Sigttou, Tcsetpgrp};
use lumen_signal::Signal;
use lumen_tty::{
    BackgroundWrite, Decision, Flush, Input, LineDiscipline, Output, Read, ReadTimer, Termios, WriterFacts,
};
use lumen_vfs::{AccessMode, PollGuard, PollMask, PollWaker};

use crate::linux::process::{self, Process};
use crate::linux::signal::ERESTARTSYS;
use crate::linux::{errno, general, ioctl};
use crate::sched::{WaitQueue, Wake};
use crate::sync::SleepMutex;
use crate::user::{copy_from_user, copy_to_user};
use crate::vfs::KernelPollHead;

const BUFFER: usize = 4096;

struct Ring(VecDeque<u8>);

impl Output for Ring {
    fn push(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > BUFFER - self.0.len() {
            return false;
        }
        self.0.extend(bytes);
        true
    }

    fn discard(&mut self) {
        self.0.clear();
    }
}

struct TtyInner {
    ld: LineDiscipline,
    to_master: Ring,
    session: Option<PidRef>,
    foreground: Option<PidRef>,
    locked: bool,
    slave_opens: usize,
    slave_seen: bool,
    hung_up: bool,
    flush: u64,
}

impl TtyInner {
    fn session(&self) -> Option<u32> {
        self.session.as_ref().map(PidRef::number)
    }

    fn foreground(&self) -> Option<u32> {
        self.foreground.as_ref().map(PidRef::number)
    }

    fn check_hangup(&self, master: bool, cmd: u32) -> Result<(), u32> {
        if !master && self.hung_up {
            Err(if cmd == ioctl::TIOCSPGRP { errno::ENOTTY } else { errno::EIO })
        } else {
            Ok(())
        }
    }
}

/// A terminal's shared discipline and session, retained independently of its descriptors.
pub struct Tty {
    lock: SleepMutex<TtyInner>,
    seq: AtomicU64,
    #[cfg(feature = "ktest")]
    detach_phase: AtomicU64,
    readers: WaitQueue,
    writers: WaitQueue,
    poll: KernelPollHead,
    pub(crate) number: u32,
}

impl Tty {
    pub(crate) fn new(number: u32) -> Result<Arc<Self>, u32> {
        let ld = LineDiscipline::try_new(Termios::console_default()).map_err(|_| errno::ENOMEM)?;
        let mut output = VecDeque::new();
        output.try_reserve_exact(BUFFER).map_err(|_| errno::ENOMEM)?;
        Ok(Arc::new(Self {
            lock: SleepMutex::new(TtyInner {
                ld,
                to_master: Ring(output),
                session: None,
                foreground: None,
                locked: true,
                slave_opens: 0,
                slave_seen: false,
                hung_up: false,
                flush: 0,
            }),
            seq: AtomicU64::new(0),
            #[cfg(feature = "ktest")]
            detach_phase: AtomicU64::new(0),
            readers: WaitQueue::new(),
            writers: WaitQueue::new(),
            poll: KernelPollHead::new(),
            number,
        }))
    }

    // Called with lock held. Over-waking is permitted by the discipline;
    // every waiter rechecks its own condition after the sequence changes.
    fn changed(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        self.readers.wake_all();
        self.writers.wake_all();
        self.poll.notify(PollMask::IN | PollMask::OUT | PollMask::RDNORM | PollMask::WRNORM | PollMask::HUP);
    }

    pub(crate) fn open_slave(self: &Arc<Self>, acquire: bool) -> Result<(), u32> {
        let mut state = self.lock.lock();
        if state.locked || state.hung_up {
            return Err(errno::EIO);
        }
        if acquire {
            let _ = self.acquire(&mut state, false);
        }
        state.slave_opens += 1;
        state.slave_seen = true;
        self.changed();
        Ok(())
    }

    pub(crate) fn close(&self, master: bool) {
        let mut state = self.lock.lock();
        if master {
            state.hung_up = true;
            state.locked = true;
            let session = state.session.clone();
            self.disassociate(&mut state);
            if let Some(leader) = session {
                process::tty_signal(leader.number() as i32, Signal::HUP);
                process::tty_signal(leader.number() as i32, Signal::CONT);
            }
            crate::vfs::devpts::remove(self);
        } else {
            assert!(state.slave_opens > 0);
            state.slave_opens -= 1;
        }
        self.changed();
    }

    fn controls(&self, process: &Process) -> bool {
        process.controlling_tty().is_some_and(|tty| core::ptr::eq(tty.as_ref(), self))
    }

    fn acquire(self: &Arc<Self>, state: &mut TtyInner, steal: bool) -> Result<(), u32> {
        let caller = process::current_process();
        process::with_tty_processes(|tree, processes| {
            let pid = caller.pid();
            if tree.sid(pid) != Some(pid) {
                return Err(errno::EPERM);
            }
            if state.session() == Some(pid) {
                return Ok(());
            }
            if caller.controlling_tty().is_some() || (state.session.is_some() && !steal) {
                return Err(errno::EPERM);
            }
            if state.session.is_some() {
                for process in processes.values() {
                    if self.controls(process) {
                        process.replace_controlling_tty(None);
                    }
                }
            }
            state.session = tree.retain_pid(pid);
            state.foreground = tree.pgid(pid).and_then(|group| tree.retain_pid(group));
            caller.replace_controlling_tty(Some(Arc::clone(self)));
            Ok(())
        })
    }

    fn disassociate(&self, state: &mut TtyInner) {
        process::with_tty_processes(|_, processes| {
            for process in processes.values() {
                if self.controls(process) {
                    process.replace_controlling_tty(None);
                }
            }
            state.session = None;
            state.foreground = None;
        });
    }

    /// Detach a departing session leader before its descriptors or pid are released.
    pub(crate) fn leader_exit(&self, pid: u32) {
        let mut state = self.lock.lock();
        if state.session() == Some(pid) {
            let foreground = state.foreground.clone();
            self.disassociate(&mut state);
            if let Some(group) = foreground {
                process::tty_signal(-(group.number() as i32), Signal::HUP);
            }
            self.changed();
        }
    }

    fn job_control(&self, state: &TtyInner, write: bool, settings: bool) -> Result<(), u32> {
        let caller = process::current_thread();
        let signal = if write { Signal::TTOU } else { Signal::TTIN };
        let (group, decision) = process::with_tty_processes(|tree, _| {
            let process = caller.process();
            let group = tree.pgid(process.pid()).expect("live tty caller");
            let facts = WriterFacts {
                controlling_terminal: self.controls(process),
                background: state.foreground().is_some_and(|foreground| foreground != group),
                signal_blocked: caller.blocked().contains(signal),
                signal_ignored: process.action(signal).handler == 1,
                orphaned_group: tree.is_orphaned_pgrp(group),
            };
            let decision = if write {
                lumen_tty::background_write_signal(settings || state.ld.termios().c_lflag & general::TOSTOP != 0, facts)
            } else if !facts.controlling_terminal || !facts.background {
                BackgroundWrite::Allow
            } else if facts.signal_blocked || facts.signal_ignored || facts.orphaned_group {
                BackgroundWrite::Eio
            } else {
                BackgroundWrite::SendSigttou
            };
            let group = matches!(decision, BackgroundWrite::SendSigttou)
                .then(|| tree.retain_pid(group).expect("live caller group"));
            (group, decision)
        });
        match decision {
            BackgroundWrite::Allow => Ok(()),
            BackgroundWrite::Eio => Err(errno::EIO),
            BackgroundWrite::SendSigttou => {
                process::tty_signal(-(group.expect("signal group").number() as i32), signal);
                Err(ERESTARTSYS)
            }
        }
    }

    pub(crate) fn poll(
        &self,
        master: bool,
        mode: AccessMode,
        waker: Option<&Arc<dyn PollWaker>>,
    ) -> (PollMask, Option<PollGuard>) {
        let guard = waker.map(|waker| self.poll.subscribe(Arc::clone(waker)));
        let state = self.lock.lock();
        let mut mask = PollMask::EMPTY;
        let readable = if master { !state.to_master.0.is_empty() } else { state.ld.poll_readable() };
        let writable = if master { state.ld.accepts(0) } else { state.to_master.0.len() < BUFFER };
        if mode.can_read() && readable {
            mask |= PollMask::IN | PollMask::RDNORM;
        }
        if mode.can_write() && writable && !state.hung_up {
            mask |= PollMask::OUT | PollMask::WRNORM;
        }
        if state.hung_up || (master && state.slave_seen && state.slave_opens == 0) {
            mask |= PollMask::HUP;
        }
        (mask, guard)
    }

    pub(crate) fn read(&self, master: bool, address: u64, count: u64, nonblock: bool) -> Result<u64, u32> {
        let want = count.min(BUFFER as u64) as usize;
        if want == 0 {
            return Ok(0);
        }
        let mut buffer = [0; BUFFER];
        let mut timer = ReadTimer::new();
        let mut deadline = None;
        loop {
            let mut state = self.lock.lock();
            let n = if master {
                if state.to_master.0.is_empty() {
                    if state.slave_seen && state.slave_opens == 0 {
                        return Err(errno::EIO);
                    }
                    None
                } else {
                    let n = want.min(state.to_master.0.len());
                    for byte in &mut buffer[..n] {
                        *byte = state.to_master.0.pop_front().expect("queued output");
                    }
                    Some(n)
                }
            } else {
                if state.hung_up {
                    return Ok(0);
                }
                self.job_control(&state, false, false)?;
                let decision = state.ld.read_decision(want, &mut timer);
                let available_nonblock =
                    nonblock && state.ld.termios().c_lflag & general::ICANON == 0 && state.ld.readable_bytes() != 0;
                match decision {
                    Decision::Read => Some(match state.ld.read(&mut buffer[..want]).read {
                        Read::Bytes(n) => n.get(),
                        _ => 0,
                    }),
                    _ if available_nonblock => Some(match state.ld.read(&mut buffer[..want]).read {
                        Read::Bytes(n) => n.get(),
                        _ => 0,
                    }),
                    Decision::WaitWithTimer { deciseconds, .. } => {
                        deadline =
                            Some(crate::time::now_ns().saturating_add(u64::from(deciseconds.get()) * 100_000_000));
                        None
                    }
                    Decision::Wait => None,
                }
            };
            if let Some(n) = n {
                self.changed();
                drop(state);
                copy_to_user(address, &buffer[..n]).map_err(|_| errno::EFAULT)?;
                return Ok(n as u64);
            }
            if nonblock {
                return Err(errno::EAGAIN);
            }
            let seen = self.seq.load(Ordering::Acquire);
            drop(state);
            match self.readers.wait_interruptible(deadline, || self.seq.load(Ordering::Acquire) != seen) {
                Wake::Interrupted => return Err(ERESTARTSYS),
                Wake::TimedOut => {
                    timer.expire();
                    deadline = None;
                }
                _ => {}
            }
        }
    }

    pub(crate) fn write(&self, master: bool, address: u64, count: u64, nonblock: bool) -> Result<u64, u32> {
        let limit = count.min(crate::linux::syscall::MAX_SINGLE_IO as u64) as usize;
        let mut buffer = [0; BUFFER];
        let mut done = 0;
        let initial_flush = self.lock.lock().flush;
        while done < limit {
            let want = (limit - done).min(BUFFER);
            let pointer = address.checked_add(done as u64).ok_or(errno::EFAULT)?;
            if copy_from_user(&mut buffer[..want], pointer).is_err() {
                return partial(done, errno::EFAULT);
            }
            let mut used = 0;
            while used < want {
                let mut state = self.lock.lock();
                if state.hung_up {
                    return partial(done, errno::EIO);
                }
                if master && state.flush != initial_flush {
                    return Ok(limit as u64);
                }
                if !master && let Err(error) = self.job_control(&state, true, false) {
                    return partial(done, error);
                }
                let n = if master {
                    let mut n = 0;
                    while used + n < want && state.ld.accepts(buffer[used + n]) {
                        let TtyInner { ld, to_master, .. } = &mut *state;
                        let effects = ld.receive(buffer[used + n], to_master);
                        n += 1;
                        if let Input::Signal(signal) = effects.input {
                            if let Some(group) = &state.foreground {
                                process::tty_signal(
                                    -(group.number() as i32),
                                    Signal::new(signal.number()).expect("job signal"),
                                );
                            }
                            if signal.flush {
                                state.flush = state.flush.wrapping_add(1);
                                self.changed();
                                return Ok(limit as u64);
                            }
                        }
                    }
                    n
                } else {
                    let TtyInner { ld, to_master, .. } = &mut *state;
                    ld.write(&buffer[used..want], to_master).consumed
                };
                done += n;
                used += n;
                if n != 0 {
                    self.changed();
                }
                if used == want {
                    break;
                }
                if nonblock {
                    return partial(done, errno::EAGAIN);
                }
                let seen = self.seq.load(Ordering::Acquire);
                drop(state);
                if self.writers.wait_interruptible(None, || self.seq.load(Ordering::Acquire) != seen)
                    == Wake::Interrupted
                {
                    return partial(done, ERESTARTSYS);
                }
            }
        }
        Ok(done as u64)
    }

    // Pause only this terminal's armed non-leader detach, with no spinlock
    // held. Another thread can complete setsid and acquire a different tty.
    #[cfg(feature = "ktest")]
    fn detach_window(&self) {
        if self.detach_phase.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            while self.detach_phase.load(Ordering::SeqCst) != 3 {
                crate::time::sleep(1_000_000);
            }
        }
    }

    fn pgrp_context(&self, state: &TtyInner, caller: &process::UserThread) -> (Option<Controlling>, Sigttou) {
        let controlling = self
            .controls(caller.process())
            .then_some(Controlling { session: state.session(), foreground: state.foreground() });
        let disposition =
            if caller.blocked().contains(Signal::TTOU) || caller.process().action(Signal::TTOU).handler == 1 {
                Sigttou::BlockedOrIgnored
            } else {
                Sigttou::Delivered
            };
        (controlling, disposition)
    }

    /// Linux x86-64 tty requests; user buffers are copied outside Lock 9a.
    pub(crate) fn ioctl(self: &Arc<Self>, master: bool, cmd: u32, arg: u64) -> Result<u64, u32> {
        // Endpoint checks, hangup and settings job control precede argument
        // validation. Drop Lock 9a for user copies, then revalidate before mutation.
        {
            let state = self.lock.lock();
            state.check_hangup(master, cmd)?;
            if (master && cmd == ioctl::TIOCNOTTY) || (!master && cmd == ioctl::TIOCSPTLCK) {
                return Err(errno::ENOTTY);
            }
            if matches!(cmd, ioctl::TCSETS | ioctl::TCSETSW | ioctl::TCSETSF | ioctl::TCFLSH) {
                self.job_control(&state, true, true)?;
            }
            if cmd == ioctl::TIOCSPGRP {
                let caller = process::current_thread();
                let group = process::with_tty_processes(|tree, _| {
                    let (controlling, disposition) = self.pgrp_context(&state, &caller);
                    tree.tcsetpgrp_job_control(caller.process().pid(), controlling, disposition)
                        .map(|group| group.map(|group| tree.retain_pid(group).expect("live caller group")))
                        .map_err(proc_error)
                })?;
                if let Some(group) = group {
                    process::tty_signal(-(group.number() as i32), Signal::TTOU);
                    return Err(ERESTARTSYS);
                }
            }
        }
        let termios = if matches!(cmd, ioctl::TCSETS | ioctl::TCSETSW | ioctl::TCSETSF) {
            let bytes = input::<36>(arg)?;
            Some(Termios {
                c_iflag: u32::from_ne_bytes(bytes[0..4].try_into().unwrap()),
                c_oflag: u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
                c_cflag: u32::from_ne_bytes(bytes[8..12].try_into().unwrap()),
                c_lflag: u32::from_ne_bytes(bytes[12..16].try_into().unwrap()),
                c_line: bytes[16],
                c_cc: bytes[17..36].try_into().unwrap(),
            })
        } else {
            None
        };
        let integer =
            if matches!(cmd, ioctl::TIOCSPGRP | ioctl::TIOCSPTLCK) { i32::from_ne_bytes(input::<4>(arg)?) } else { 0 };
        let winsize = if cmd == ioctl::TIOCSWINSZ {
            let bytes = input::<8>(arg)?;
            Some(lumen_tty::Winsize {
                ws_row: u16::from_ne_bytes(bytes[0..2].try_into().unwrap()),
                ws_col: u16::from_ne_bytes(bytes[2..4].try_into().unwrap()),
                ws_xpixel: u16::from_ne_bytes(bytes[4..6].try_into().unwrap()),
                ws_ypixel: u16::from_ne_bytes(bytes[6..8].try_into().unwrap()),
            })
        } else {
            None
        };
        let mut out = [0; 36];
        let mut length = 0;
        let mut state = self.lock.lock();
        state.check_hangup(master, cmd)?;
        match cmd {
            ioctl::TCGETS => {
                let termios = state.ld.termios();
                for (chunk, word) in out[..16].as_chunks_mut::<4>().0.iter_mut().zip([
                    termios.c_iflag,
                    termios.c_oflag,
                    termios.c_cflag,
                    termios.c_lflag,
                ]) {
                    chunk.copy_from_slice(&word.to_ne_bytes());
                }
                out[16] = termios.c_line;
                out[17..].copy_from_slice(&termios.c_cc);
                length = 36;
            }
            ioctl::TCSETS | ioctl::TCSETSW | ioctl::TCSETSF => {
                self.job_control(&state, true, true)?;
                let flush = if cmd == ioctl::TCSETSF { Flush::Input } else { Flush::Keep };
                let _ = state.ld.set_termios(termios.unwrap(), flush);
                if flush == Flush::Input {
                    state.flush = state.flush.wrapping_add(1);
                }
                self.changed();
            }
            ioctl::TCFLSH => {
                self.job_control(&state, true, true)?;
                if arg > 2 {
                    return Err(errno::EINVAL);
                }
                if arg != 1 {
                    let _ = state.ld.flush_input();
                    state.flush = state.flush.wrapping_add(1);
                }
                if arg != 0 {
                    state.to_master.discard();
                }
                self.changed();
            }
            ioctl::TCSBRK | ioctl::TCXONC => {}
            ioctl::TIOCGWINSZ => {
                let size = state.ld.winsize();
                for (chunk, value) in out[..8].as_chunks_mut::<2>().0.iter_mut().zip([
                    size.ws_row,
                    size.ws_col,
                    size.ws_xpixel,
                    size.ws_ypixel,
                ]) {
                    chunk.copy_from_slice(&value.to_ne_bytes());
                }
                length = 8;
            }
            ioctl::TIOCSWINSZ => {
                if state.ld.set_winsize(winsize.unwrap())
                    && let Some(group) = &state.foreground
                {
                    process::tty_signal(-(group.number() as i32), Signal::WINCH);
                }
                self.changed();
            }
            ioctl::TIOCSCTTY => {
                if state.hung_up {
                    return Err(errno::EIO);
                }
                self.acquire(&mut state, arg == 1)?;
            }
            ioctl::TIOCNOTTY => {
                let caller = process::current_process();
                if !self.controls(&caller) {
                    return Err(errno::ENOTTY);
                }
                if state.session() == Some(caller.pid()) {
                    if let Some(group) = &state.foreground {
                        process::tty_signal(-(group.number() as i32), Signal::HUP);
                        process::tty_signal(-(group.number() as i32), Signal::CONT);
                    }
                    self.disassociate(&mut state);
                } else {
                    #[cfg(feature = "ktest")]
                    self.detach_window();
                    process::with_tty_processes(|_, _| {
                        // setsid and a new acquisition may have completed since
                        // the first controls check. Revalidate while holding PROCS.
                        if self.controls(&caller) {
                            caller.replace_controlling_tty(None);
                        }
                    });
                }
                self.changed();
            }
            ioctl::TIOCSPGRP => {
                let caller = process::current_thread();
                let result = process::with_tty_processes(|tree, _| -> Result<Option<PidRef>, u32> {
                    let (controlling, disposition) = self.pgrp_context(&state, &caller);
                    let result = tree
                        .tcsetpgrp(caller.process().pid(), controlling, integer, disposition)
                        .map_err(proc_error)?;
                    match result {
                        Tcsetpgrp::Set(group) => {
                            state.foreground = Some(tree.retain_pid(group).expect("validated foreground identity"));
                            Ok(None)
                        }
                        Tcsetpgrp::Sigttou { pgrp } => Ok(Some(tree.retain_pid(pgrp).expect("live caller group"))),
                    }
                })?;
                if let Some(group) = result {
                    process::tty_signal(-(group.number() as i32), Signal::TTOU);
                    return Err(ERESTARTSYS);
                }
                self.changed();
            }
            ioctl::TIOCGPGRP | ioctl::TIOCGSID => {
                if !master && !self.controls(&process::current_process()) {
                    return Err(errno::ENOTTY);
                }
                let value = if cmd == ioctl::TIOCGSID {
                    state.session().ok_or(errno::ENOTTY)?
                } else {
                    state.foreground().unwrap_or(0)
                };
                out[..4].copy_from_slice(&value.to_ne_bytes());
                length = 4;
            }
            ioctl::FIONREAD | ioctl::TIOCOUTQ => {
                let n = if (cmd == ioctl::FIONREAD) == master {
                    state.to_master.0.len()
                } else {
                    state.ld.readable_bytes()
                };
                out[..4].copy_from_slice(&(n as u32).to_ne_bytes());
                length = 4;
            }
            ioctl::TIOCGPTN | ioctl::TIOCGPTLCK if master => {
                let value = if cmd == ioctl::TIOCGPTN { self.number } else { u32::from(state.locked) };
                out[..4].copy_from_slice(&value.to_ne_bytes());
                length = 4;
            }
            ioctl::TIOCSPTLCK if master => state.locked = integer != 0,
            ioctl::TIOCPKT => return Err(errno::EINVAL),
            _ => return Err(errno::ENOTTY),
        }
        drop(state);
        if length != 0 {
            copy_to_user(arg, &out[..length]).map_err(|_| errno::EFAULT)?;
        }
        Ok(0)
    }
}

fn proc_error(error: lumen_proc::Error) -> u32 {
    match error {
        lumen_proc::Error::User(error) => error.to_errno(),
        lumen_proc::Error::Bug(bug) => panic!("tty process tree: {bug}"),
    }
}

fn input<const N: usize>(address: u64) -> Result<[u8; N], u32> {
    let mut bytes = [0; N];
    copy_from_user(&mut bytes, address).map_err(|_| errno::EFAULT)?;
    Ok(bytes)
}

fn partial(done: usize, error: u32) -> Result<u64, u32> {
    if done == 0 { Err(error) } else { Ok(done as u64) }
}

crate::ktest!(
    fn pty_waits_backpressure_and_copy_faults() {
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let tty = Tty::new(255).map_err(|_| "tty")?;
            let mut raw = Termios::console_default();
            raw.c_iflag = 0;
            raw.c_oflag = 0;
            raw.c_lflag = 0;
            raw.c_cc[general::VMIN as usize] = 2;
            raw.c_cc[general::VTIME as usize] = 0;
            {
                let mut state = tty.lock.lock();
                state.locked = false;
                let _ = state.ld.set_termios(raw, Flush::Input);
            }
            tty.open_slave(false).map_err(|_| "open slave")?;
            struct Spy(AtomicU64);
            impl PollWaker for Spy {
                fn wake(&self, _: PollMask) {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
            let spy = Arc::new(Spy(AtomicU64::new(0)));
            let waker: Arc<dyn PollWaker> = spy.clone();
            let (_, guard) = tty.poll(false, AccessMode::ReadOnly, Some(&waker));
            let base = process::KTEST_USER_VA;
            copy_to_user(base, b"ab").map_err(|_| "input")?;
            kassert_eq!(tty.write(true, base, 1, false), Ok(1));
            kassert!(spy.0.load(Ordering::Relaxed) > 0);
            drop(guard);
            kassert_eq!(tty.poll.subscriber_count(), 0);
            kassert!(!tty.poll(false, AccessMode::ReadOnly, None).0.intersects(PollMask::IN));
            let reader_tty = tty.clone();
            let reader = process::spawn_test_thread(move |_| reader_tty.read(false, base + 128, 2, false))?;
            while !reader.blocked() {
                crate::time::sleep(1_000_000);
            }
            kassert_eq!(tty.write(true, base + 1, 1, false), Ok(1));
            kassert_eq!(reader.join()?, Ok(2));
            let mut got = [0; 2];
            copy_from_user(&mut got, base + 128).map_err(|_| "read result")?;
            kassert_eq!(got, *b"ab");

            // A copy fault loses precisely the chunk already consumed.
            kassert_eq!(tty.write(true, base, 2, false), Ok(2));
            kassert_eq!(tty.read(false, 0, 2, true), Err(errno::EFAULT));
            kassert_eq!(tty.read(false, base + 128, 2, true), Err(errno::EAGAIN));

            // A full output ring blocks the next slave write; a master read
            // wakes it. Raw mode makes the output capacity byte-exact.
            copy_to_user(base, &[b'x'; BUFFER]).map_err(|_| "fill")?;
            kassert_eq!(tty.write(false, base, BUFFER as u64, true), Ok(BUFFER as u64));
            kassert_eq!(tty.write(false, base, 1, true), Err(errno::EAGAIN));
            kassert!(!tty.poll(false, AccessMode::WriteOnly, None).0.intersects(PollMask::OUT));
            let writer_tty = tty.clone();
            let writer = process::spawn_test_thread(move |_| writer_tty.write(false, base, 1, false))?;
            while !writer.blocked() {
                crate::time::sleep(1_000_000);
            }
            kassert_eq!(tty.read(true, base + 4096, 1, false), Ok(1));
            kassert_eq!(writer.join()?, Ok(1));
            kassert_eq!(tty.read(true, base + 4096, BUFFER as u64, false), Ok(BUFFER as u64));

            let reader_tty = tty.clone();
            let reader = process::spawn_test_thread(move |_| reader_tty.read(false, base + 128, 2, false))?;
            while !reader.blocked() {
                crate::time::sleep(1_000_000);
            }
            tty.close(true);
            kassert_eq!(reader.join()?, Ok(0));
            kassert_eq!(tty.write(false, base, 1, true), Err(errno::EIO));
            kassert!(tty.poll(false, AccessMode::ReadWrite, None).0.intersects(PollMask::HUP));
            let before = tty.lock.lock().ld.termios();
            let sequence = tty.seq.load(Ordering::Acquire);
            // Valid and invalid pointers both see hangup first. Setters must
            // leave discipline state and notification sequence unchanged.
            copy_to_user(base, &[0; 36]).map_err(|_| "hung-up ioctl input")?;
            for arg in [base, 0] {
                for cmd in [ioctl::TCGETS, ioctl::TCSETS, ioctl::TCSETSW, ioctl::TCSETSF, ioctl::TIOCSPTLCK] {
                    kassert_eq!(tty.ioctl(false, cmd, arg), Err(errno::EIO));
                }
                kassert_eq!(tty.ioctl(false, ioctl::TIOCSPGRP, arg), Err(errno::ENOTTY));
            }
            kassert_eq!(tty.lock.lock().ld.termios(), before);
            kassert_eq!(tty.seq.load(Ordering::Acquire), sequence);
            let mut untouched = [1; 36];
            copy_from_user(&mut untouched, base).map_err(|_| "hung-up output")?;
            kassert_eq!(untouched, [0; 36]);
            tty.close(false);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn pty_readable_masks_and_blocked_poller() {
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let tty = Tty::new(255).map_err(|_| "tty")?;
            tty.lock.lock().locked = false;
            tty.open_slave(false).map_err(|_| "open slave")?;
            let base = process::KTEST_USER_VA;
            let readable = PollMask::IN | PollMask::RDNORM;
            copy_to_user(base, b"line\n").map_err(|_| "line")?;
            kassert_eq!(tty.poll(false, AccessMode::ReadOnly, None).0 & readable, PollMask::EMPTY);
            kassert_eq!(tty.write(true, base, 5, false), Ok(5));
            // These positive masks must fail if readable readiness is suppressed.
            kassert_eq!(tty.poll(false, AccessMode::ReadOnly, None).0 & readable, readable);
            kassert_eq!(tty.poll(true, AccessMode::ReadOnly, None).0 & readable, readable);
            kassert_eq!(tty.read(false, base + 128, 5, false), Ok(5));
            kassert_eq!(tty.read(true, base + 128, 6, false), Ok(6));

            struct PollWait {
                sequence: AtomicU64,
                queue: WaitQueue,
            }
            impl PollWaker for PollWait {
                fn wake(&self, _: PollMask) {
                    self.sequence.fetch_add(1, Ordering::Release);
                    self.queue.wake_all();
                }
            }
            // Both ends: confirm the poller is blocked before making it readable.
            for master in [false, true] {
                let poll_tty = tty.clone();
                let poller = process::spawn_test_thread(move |_| {
                    let wait = Arc::new(PollWait { sequence: AtomicU64::new(0), queue: WaitQueue::new() });
                    let waker: Arc<dyn PollWaker> = wait.clone();
                    loop {
                        let seen = wait.sequence.load(Ordering::Acquire);
                        let (mask, guard) = poll_tty.poll(master, AccessMode::ReadOnly, Some(&waker));
                        if mask.intersects(readable) {
                            return mask & readable;
                        }
                        wait.queue.wait_until(|| wait.sequence.load(Ordering::Acquire) != seen);
                        drop(guard);
                    }
                })?;
                while !poller.blocked() {
                    crate::time::sleep(1_000_000);
                }
                kassert_eq!(tty.poll.subscriber_count(), 1);
                kassert_eq!(tty.write(!master, base, 5, false), Ok(5));
                kassert_eq!(poller.join()?, readable);
                kassert_eq!(tty.poll.subscriber_count(), 0);
                if !master {
                    kassert_eq!(tty.read(false, base + 128, 5, false), Ok(5));
                }
                kassert_eq!(tty.read(true, base + 128, 6, false), Ok(6));
            }
            tty.close(true);
            tty.close(false);
            kassert!(tty.poll(false, AccessMode::ReadOnly, None).0.intersects(PollMask::HUP));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn pty_detach_preserves_a_new_controlling_terminal() {
        use crate::{kassert, kassert_eq};
        use lumen_signal::{Blocked, SigSet};
        use lumen_vfs::OpenFlags;

        fn open(path: &[u8]) -> Result<process::KernelFile, &'static str> {
            let flags = OpenFlags::from_bits(general::O_RDWR | general::O_NOCTTY);
            let file = process::open_kernel_file(path, flags.bits(), 0).map_err(|_| "open terminal")?;
            crate::vfs::devpts::open(file, flags).map_err(|_| "terminal open hook")
        }

        process::in_test_process(|parent| -> crate::ktest::TestResult {
            process::set_current_blocked(Blocked::new(SigSet::of(&[Signal::HUP, Signal::CONT])));
            let old_master = open(b"/dev/ptmx")?;
            let new_master = open(b"/dev/ptmx")?;
            let old = old_master.tty().ok_or("old endpoint")?.tty.clone();
            let new = new_master.tty().ok_or("new endpoint")?.tty.clone();
            old.lock.lock().locked = false;
            new.lock.lock().locked = false;
            kassert_eq!(old.ioctl(false, ioctl::TIOCSCTTY, 0), Ok(0));

            // A non-racing non-leader detach must still clear its own association.
            let inherited = old.clone();
            process::in_test_child(move |_| {
                let caller = process::current_process();
                kassert!(inherited.controls(&caller));
                kassert_eq!(inherited.ioctl(false, ioctl::TIOCNOTTY, 0), Ok(0));
                kassert!(caller.controlling_tty().is_none());
                Ok(())
            })?;
            kassert!(old.controls(&process::current_process()));

            let inherited = old.clone();
            let acquired = new.clone();
            let child_result = process::in_test_child(move |pid| {
                let caller = process::current_process();
                kassert!(inherited.controls(&caller));
                inherited.detach_phase.store(1, Ordering::SeqCst);
                let detaching = inherited.clone();
                let worker = process::spawn_test_thread(move |_| detaching.ioctl(false, ioctl::TIOCNOTTY, 0))?;
                while inherited.detach_phase.load(Ordering::SeqCst) != 2 && !worker.finished() {
                    crate::time::sleep(1_000_000);
                }
                // Keep errors until after releasing and joining the paused thread.
                let switched = (|| -> crate::ktest::TestResult {
                    kassert_eq!(inherited.detach_phase.load(Ordering::SeqCst), 2);
                    kassert_eq!(process::setsid(), Ok(pid));
                    kassert_eq!(acquired.ioctl(false, ioctl::TIOCSCTTY, 0), Ok(0));
                    kassert!(acquired.controls(&caller));
                    Ok(())
                })();
                inherited.detach_phase.store(3, Ordering::SeqCst);
                let detached = worker.join();
                switched?;
                kassert_eq!(detached?, Ok(0));
                // This assertion kills clearing without the PROCS-held recheck.
                kassert!(acquired.controls(&caller));
                let alias = open(b"/dev/tty")?;
                kassert!(Arc::ptr_eq(&alias.tty().ok_or("alias endpoint")?.tty, &acquired));
                kassert_eq!(acquired.lock.lock().session(), Some(pid));
                kassert_eq!(acquired.lock.lock().foreground(), Some(pid));
                Ok(())
            });
            // The child's leader exit must release the new tty's reservations.
            let released = {
                let state = new.lock.lock();
                state.session.is_none() && state.foreground.is_none()
            };
            let parent_unchanged =
                old.controls(&process::current_process()) && old.lock.lock().session() == Some(parent);
            // Clean up even when testing a mutant that strands the new session.
            new.disassociate(&mut new.lock.lock());
            old.disassociate(&mut old.lock.lock());
            drop(new_master);
            drop(old_master);
            child_result?;
            kassert!(released);
            kassert!(parent_unchanged);
            Ok(())
        })?
    }
);
