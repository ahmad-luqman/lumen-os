//! Terminal line disciplines, readiness and controlling-terminal operations.
//!
//! The sleep lock is Lock 9a: never held across user copies or other VFS
//! locks. Process-tree and signal locks may nest beneath it. Sequence changes
//! are published under this lock before waking readers, writers and pollers.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lumen_proc::{Controlling, PidRef, Sigttou, Tcsetpgrp};
use lumen_signal::Signal;
use lumen_tty::{
    BackgroundWrite, Decision, Flush, Input, LineDiscipline, Output, Read, ReadTimer, Termios, WriterFacts,
};
use lumen_vfs::{AccessMode, PollDefer, PollGuard, PollMask, PollWaker};

use crate::linux::process::{self, ConsoleIo, Process};
use crate::linux::signal::ERESTARTSYS;
use crate::linux::{errno, general, ioctl};
use crate::sched::{WaitQueue, Wake};
use crate::sync::SleepMutex;
use crate::user::{copy_from_user, copy_to_user};
use crate::vfs::KernelPollHead;

const BUFFER: usize = 4096;
const VT_OUTPUT_BATCH: usize = 256;

struct Ring(VecDeque<u8>, Option<ConsoleOutput>);

impl Ring {
    fn write(&mut self, ld: &mut LineDiscipline, bytes: &[u8]) -> usize {
        if let Some(console) = &mut self.1
            && matches!(console.io, ConsoleIo::Vt(_))
        {
            let mut batch = VtOutputBatch { console, bytes: [0; VT_OUTPUT_BATCH], len: 0 };
            let consumed = ld.write(bytes, &mut batch).consumed;
            batch.flush();
            consumed
        } else {
            ld.write(bytes, self).consumed
        }
    }
}

impl Output for Ring {
    fn push(&mut self, bytes: &[u8]) -> bool {
        if let Some(console) = &mut self.1 {
            if console.io == ConsoleIo::Serial {
                crate::klog::with_line_lock(|| console.write(bytes));
            } else {
                console.write(bytes);
            }
            return true;
        }
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
    pending: [u8; 4],
    pending_len: usize,
    pending_pos: usize,
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
    console: Option<ConsoleIo>,
    active: AtomicBool,
}

impl Tty {
    pub(crate) fn new(number: u32) -> Result<Arc<Self>, u32> {
        Self::create(number, None)
    }

    fn create(number: u32, console: Option<ConsoleIo>) -> Result<Arc<Self>, u32> {
        let ld = LineDiscipline::try_new(Termios::console_default()).map_err(|_| errno::ENOMEM)?;
        let mut output = VecDeque::new();
        if console.is_none() {
            output.try_reserve_exact(BUFFER).map_err(|_| errno::ENOMEM)?;
        }
        Ok(Arc::new(Self {
            lock: SleepMutex::new(TtyInner {
                ld,
                to_master: Ring(output, console.map(ConsoleOutput::new)),
                session: None,
                foreground: None,
                locked: console.is_none(),
                slave_opens: 0,
                slave_seen: false,
                hung_up: false,
                flush: 0,
                pending: [0; 4],
                pending_len: 0,
                pending_pos: 0,
            }),
            seq: AtomicU64::new(0),
            #[cfg(feature = "ktest")]
            detach_phase: AtomicU64::new(0),
            readers: WaitQueue::new(),
            writers: WaitQueue::new(),
            poll: KernelPollHead::new(),
            number,
            console,
            active: AtomicBool::new(false),
        }))
    }

    pub(crate) fn is_console(&self) -> bool {
        self.console.is_some()
    }

    // Lock 9a serializes the input thread's staging buffer with all flushes.
    fn flush_driver(&self, state: &mut TtyInner) {
        state.flush = state.flush.wrapping_add(1);
        state.pending_len = 0;
        state.pending_pos = 0;
        match self.console {
            Some(ConsoleIo::Serial) => crate::serial_in::discard_input(),
            Some(ConsoleIo::Vt(vt)) => crate::vt::discard_input(vt.index()),
            _ => {}
        }
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
        let guard = waker.map(|waker| {
            let waker: Arc<dyn PollWaker> = if self.console == Some(ConsoleIo::Serial) {
                Arc::new(ConsolePoll(Arc::clone(waker)))
            } else {
                Arc::clone(waker)
            };
            self.poll.subscribe(waker)
        });
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
                                self.flush_driver(&mut state);
                                self.changed();
                                return Ok(limit as u64);
                            }
                        }
                    }
                    n
                } else {
                    let TtyInner { ld, to_master, .. } = &mut *state;
                    to_master.write(ld, &buffer[used..want])
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
                    self.flush_driver(&mut state);
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
                    self.flush_driver(&mut state);
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

/// Console output is synchronous and already processed by the line discipline.
/// The VT parser retains split UTF-8 codepoints and escape sequences.
struct ConsoleOutput {
    io: ConsoleIo,
}

/// Batch a discipline write's processed VT bytes without allocating or changing
/// them. Flush before releasing Lock 9a; input echo keeps its immediate path.
struct VtOutputBatch<'a> {
    console: &'a mut ConsoleOutput,
    bytes: [u8; VT_OUTPUT_BATCH],
    len: usize,
}

impl VtOutputBatch<'_> {
    fn flush(&mut self) {
        if self.len != 0 {
            self.console.write(&self.bytes[..self.len]);
            self.len = 0;
        }
    }
}

impl Output for VtOutputBatch<'_> {
    fn push(&mut self, mut bytes: &[u8]) -> bool {
        while !bytes.is_empty() {
            let n = bytes.len().min(self.bytes.len() - self.len);
            self.bytes[self.len..self.len + n].copy_from_slice(&bytes[..n]);
            self.len += n;
            bytes = &bytes[n..];
            if self.len == self.bytes.len() {
                self.flush();
            }
        }
        true
    }

    fn discard(&mut self) {
        self.len = 0;
    }
}

impl ConsoleOutput {
    fn new(io: ConsoleIo) -> Self {
        Self { io }
    }

    /// The serial caller holds the log line lock across this dispatch.
    fn write(&mut self, bytes: &[u8]) {
        match self.io {
            ConsoleIo::Serial => crate::arch::serial::tty_bytes(bytes),
            ConsoleIo::Vt(vt) => {
                let _ = crate::term::write_bytes_to(vt.index(), bytes);
            }
            ConsoleIo::Capture => unreachable!("capture has no Tty"),
        }
    }
}

/// Exercise the production console writer under the loopback test's line lock.
#[cfg(feature = "ktest")]
pub(crate) fn serial_output_held_for_test(bytes: &[u8]) {
    ConsoleOutput::new(ConsoleIo::Serial).write(bytes);
}

static CONSOLES: SleepMutex<[Option<Arc<Tty>>; 5]> = SleepMutex::new([const { None }; 5]);

/// Create the consoles before process frame accounting and before any shell starts.
pub(crate) fn init() {
    let mut consoles = CONSOLES.lock();
    for (index, slot) in consoles.iter_mut().enumerate() {
        let io = if index == 0 {
            ConsoleIo::Serial
        } else {
            ConsoleIo::Vt(process::VtId::new(index - 1).expect("console index"))
        };
        let tty = Tty::create(index as u32, Some(io)).expect("console tty allocation");
        if index != 0
            && let Some(status) = crate::term::status()
        {
            let _ = tty.lock.lock().ld.set_winsize(lumen_tty::Winsize {
                ws_row: status.rows as u16,
                ws_col: status.cols as u16,
                ws_xpixel: 0,
                ws_ypixel: 0,
            });
        }
        *slot = Some(tty);
    }
}

/// The physical console selected by the root runner, independent of its controlling terminal.
pub(crate) fn console(io: ConsoleIo) -> Option<Arc<Tty>> {
    let index = match io {
        ConsoleIo::Capture => return None,
        ConsoleIo::Serial => 0,
        ConsoleIo::Vt(vt) => vt.index() + 1,
    };
    CONSOLES.lock()[index].clone()
}

/// Serial diagnostics count notifications reaching terminal poll subscriptions.
struct ConsolePoll(Arc<dyn PollWaker>);

impl PollWaker for ConsolePoll {
    fn wake(&self, ready: PollMask, defer: &mut PollDefer) {
        crate::serial_in::note_poll_woken();
        self.0.wake(ready, defer);
    }
}

pub(crate) fn serial_subscribers() -> usize {
    console(ConsoleIo::Serial).map_or(0, |tty| tty.poll.subscriber_count())
}

struct InputOwner {
    tty: Arc<Tty>,
    ready: AtomicBool,
    stop: AtomicBool,
    started: WaitQueue,
}

/// Exclusive input ownership until the root runner has received its exit result.
/// Dropping it wakes and joins the input thread before the kernel shell resumes.
pub(crate) struct ConsoleSession {
    owner: Arc<InputOwner>,
    thread: Option<crate::sched::JoinHandle>,
}

impl ConsoleSession {
    pub(crate) fn prepare(io: ConsoleIo) -> Result<Option<Self>, &'static str> {
        let Some(tty) = console(io) else { return Ok(None) };
        if tty.active.swap(true, Ordering::AcqRel) {
            return Err("console already has an input owner");
        }
        let owner = Arc::new(InputOwner {
            tty,
            ready: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            started: WaitQueue::new(),
        });
        let input = owner.clone();
        let name = match io {
            ConsoleIo::Serial => "tty-serial",
            ConsoleIo::Vt(vt) => ["tty-vt1", "tty-vt2", "tty-vt3", "tty-vt4"][vt.index()],
            _ => unreachable!("console number"),
        };
        let thread = match crate::sched::spawn(name, move || input.run()) {
            Ok(thread) => thread,
            Err(_) => {
                owner.tty.active.store(false, Ordering::Release);
                return Err("cannot start console input thread");
            }
        };
        Ok(Some(Self { owner, thread: Some(thread) }))
    }

    /// Bind the newly registered session leader before accepting signal characters.
    pub(crate) fn activate(&self) {
        let tty = &self.owner.tty;
        tty.acquire(&mut tty.lock.lock(), true).expect("new root is a session leader without a tty");
        self.owner.ready.store(true, Ordering::Release);
        self.owner.started.wake_all();
    }
}

impl Drop for ConsoleSession {
    fn drop(&mut self) {
        self.owner.stop.store(true, Ordering::Release);
        self.owner.started.wake_all();
        self.owner.tty.writers.wake_all();
        crate::serial_in::wake_reader();
        crate::keyboard::wake_readers();
        if let Some(thread) = self.thread.take() {
            thread.join();
        }
        // Cooked input belongs to the departing run. Leave bytes still in
        // the hardware/router queue for the kernel shell taking ownership.
        let mut state = self.owner.tty.lock.lock();
        let _ = state.ld.flush_input();
        state.pending_len = 0;
        state.pending_pos = 0;
        self.owner.tty.changed();
        self.owner.tty.active.store(false, Ordering::Release);
    }
}

impl InputOwner {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    fn run(&self) {
        self.started.wait_until(|| self.ready.load(Ordering::Acquire) || self.stopped());
        let io = self.tty.console.expect("console input");
        while !self.stopped() {
            let mut state = self.tty.lock.lock();
            if self.stopped() {
                break;
            }
            if state.pending_pos == state.pending_len {
                state.pending_pos = 0;
                state.pending_len = match io {
                    ConsoleIo::Serial => match crate::serial_in::read_byte() {
                        Some(byte) => {
                            state.pending[0] = byte;
                            1
                        }
                        None => 0,
                    },
                    ConsoleIo::Vt(vt) => {
                        crate::vt::read_key(vt.index()).map_or(0, |key| vt_key_to_bytes(key, &mut state.pending))
                    }
                    ConsoleIo::Capture => unreachable!(),
                };
                if state.pending_len == 0 {
                    drop(state);
                    match io {
                        ConsoleIo::Serial => crate::serial_in::wait_or(|| self.stopped()),
                        ConsoleIo::Vt(vt) => crate::vt::wait_input_or(vt.index(), || self.stopped()),
                        ConsoleIo::Capture => unreachable!(),
                    }
                    continue;
                }
            }
            let byte = state.pending[state.pending_pos];
            if !state.ld.accepts(byte) {
                let seen = self.tty.seq.load(Ordering::Acquire);
                drop(state);
                self.tty.writers.wait_until(|| self.stopped() || self.tty.seq.load(Ordering::Acquire) != seen);
                continue;
            }
            state.pending_pos += 1;
            let TtyInner { ld, to_master, .. } = &mut *state;
            let effects = ld.receive(byte, to_master);
            if let Input::Signal(signal) = effects.input {
                if let Some(group) = &state.foreground {
                    process::tty_signal(-(group.number() as i32), Signal::new(signal.number()).expect("job signal"));
                }
                if signal.flush {
                    self.tty.flush_driver(&mut state);
                }
            }
            self.tty.changed();
        }
    }
}

/// Linux VT input encoding, before termios applies ICRNL/IGNCR/VERASE.
/// Kernel shells and the capture bridge retain their own key conventions.
fn vt_key_to_bytes(key: crate::keyboard::Key, out: &mut [u8; 4]) -> usize {
    match key {
        crate::keyboard::Key::Enter => {
            out[0] = b'\r';
            1
        }
        crate::keyboard::Key::Backspace => {
            out[0] = 0x7f;
            1
        }
        crate::keyboard::Key::Char(ch) => ch.encode_utf8(out).len(),
        _ => process::vt_key_to_bytes(key, out),
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
                fn wake(&self, _: PollMask, _: &mut PollDefer) {
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
                fn wake(&self, _: PollMask, _: &mut PollDefer) {
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

#[cfg(feature = "ktest")]
struct RestoreConsole(Arc<Tty>, Termios);

#[cfg(feature = "ktest")]
impl RestoreConsole {
    fn raw(tty: &Arc<Tty>) -> Self {
        let mut state = tty.lock.lock();
        let saved = state.ld.termios();
        let mut raw = saved;
        raw.c_lflag = 0;
        raw.c_iflag = 0;
        raw.c_cc[general::VMIN as usize] = 1;
        raw.c_cc[general::VTIME as usize] = 0;
        let _ = state.ld.set_termios(raw, Flush::Input);
        Self(tty.clone(), saved)
    }
}

#[cfg(feature = "ktest")]
impl Drop for RestoreConsole {
    fn drop(&mut self) {
        let mut state = self.0.lock.lock();
        let _ = state.ld.set_termios(self.1, Flush::Input);
        self.0.flush_driver(&mut state);
        self.0.changed();
    }
}

crate::ktest!(
    fn console_input_backpressure_flush_and_stop() {
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let tty = console(ConsoleIo::Serial).ok_or("serial tty")?;
            let _restore = RestoreConsole::raw(&tty);
            let session = ConsoleSession::prepare(ConsoleIo::Serial)?.ok_or("serial owner")?;
            session.activate();
            let base = process::KTEST_USER_VA;
            let fill = || {
                let mut state = tty.lock.lock();
                let TtyInner { ld, to_master, .. } = &mut *state;
                while ld.accepts(b'x') {
                    let _ = ld.receive(b'x', to_master);
                }
            };
            let staged = || {
                let state = tty.lock.lock();
                state.pending_pos != state.pending_len
            };
            fill();
            crate::serial_in::inject_for_test(b'a');
            crate::serial_in::inject_for_test(b'b');
            while !staged() {
                crate::time::sleep(1_000_000);
            }
            kassert!(crate::serial_in::available());
            kassert_eq!(tty.read(false, base, BUFFER as u64, true), Ok((BUFFER - 1) as u64));
            while tty.lock.lock().ld.readable_bytes() != 2 {
                crate::time::sleep(1_000_000);
            }
            kassert_eq!(tty.read(false, base, 2, true), Ok(2));
            let mut bytes = [0; 2];
            copy_from_user(&mut bytes, base).map_err(|_| "received bytes")?;
            kassert_eq!(bytes, *b"ab");

            // Both input-flushing ioctls discard the refused staging byte
            // and the byte still upstream, even while the input thread waits.
            for cmd in [ioctl::TCFLSH, ioctl::TCSETSF] {
                kassert_eq!(tty.ioctl(false, ioctl::TCGETS, base), Ok(0));
                fill();
                crate::serial_in::inject_for_test(b'c');
                crate::serial_in::inject_for_test(b'd');
                while !staged() {
                    crate::time::sleep(1_000_000);
                }
                kassert_eq!(tty.ioctl(false, cmd, if cmd == ioctl::TCFLSH { 0 } else { base }), Ok(0));
                kassert!(!staged());
                kassert!(!crate::serial_in::available());
                kassert_eq!(tty.read(false, base + 128, 2, true), Err(errno::EAGAIN));
            }
            crate::serial_in::inject_for_test(b'z');
            kassert_eq!(tty.read(false, base, 1, false), Ok(1));
            copy_from_user(&mut bytes[..1], base).map_err(|_| "fresh input")?;
            kassert_eq!(bytes[0], b'z');

            // Cancellation also wakes the capacity wait, not just the input wait.
            fill();
            crate::serial_in::inject_for_test(b'e');
            while !staged() {
                crate::time::sleep(1_000_000);
            }
            drop(session);
            kassert!(!tty.active.load(Ordering::Acquire));
            kassert_eq!(tty.lock.lock().ld.readable_bytes(), 0);
            kassert!(!staged());
            // A cancelled start gate must be joinable as well.
            drop(ConsoleSession::prepare(ConsoleIo::Serial)?);
            kassert!(!tty.active.load(Ordering::Acquire));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn console_descriptors_poll_and_interruptible_read() {
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::{SenderClass, SigInfo};
        use lumen_vfs::OpenFlags;
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let tty = console(ConsoleIo::Serial).ok_or("serial tty")?;
            let _restore = RestoreConsole::raw(&tty);
            let session = ConsoleSession::prepare(ConsoleIo::Serial)?.ok_or("serial owner")?;
            session.activate();
            let flags = OpenFlags::from_bits(general::O_RDWR);
            let alias = process::open_kernel_file(b"/dev/tty", flags.bits(), 0).map_err(|_| "alias inode")?;
            let alias = crate::vfs::devpts::open(alias, flags).map_err(|_| "alias open")?;
            kassert!(Arc::ptr_eq(&alias.tty().ok_or("alias tty")?.tty, &tty));
            let fd = process::install_current_fd(alias, false).map_err(|_| "install alias")?;
            let base = process::KTEST_USER_VA;
            let poll = move || {
                let mut row = [0u8; 8];
                row[..4].copy_from_slice(&fd.to_ne_bytes());
                row[4..6].copy_from_slice(&(PollMask::IN | PollMask::RDNORM).bits().to_ne_bytes());
                copy_to_user(base, &row).map_err(|_| "poll input")?;
                let mut frame = crate::arch::syscall::SyscallFrame {
                    rax: u64::from(general::__NR_poll),
                    rdi: base,
                    rsi: 1,
                    rdx: u64::MAX,
                    ..Default::default()
                };
                crate::linux::syscall::dispatch(&mut frame);
                copy_from_user(&mut row, base).map_err(|_| "poll output")?;
                Ok::<_, &'static str>((frame.rax, u16::from_ne_bytes(row[6..8].try_into().unwrap())))
            };
            kassert!(!tty.poll(false, AccessMode::ReadWrite, None).0.intersects(PollMask::IN));
            kassert!(tty.poll(false, AccessMode::ReadWrite, None).0.intersects(PollMask::OUT | PollMask::WRNORM));
            let poller = process::spawn_test_thread(move |_| poll())?;
            while !poller.blocked() {
                crate::time::sleep(1_000_000);
            }
            kassert_eq!(tty.poll.subscriber_count(), 1);
            crate::serial_in::inject_for_test(b'q');
            kassert_eq!(poller.join()??, (1, (PollMask::IN | PollMask::RDNORM).bits()));
            kassert_eq!(tty.poll.subscriber_count(), 0);
            kassert_eq!(tty.read(false, base + 128, 1, false), Ok(1));

            process::set_current_action(
                Signal::USR1,
                process::KernelSigaction { handler: 0x400100, ..Default::default() },
            );
            let tid = Arc::new(AtomicU64::new(0));
            let id = tid.clone();
            let reading = tty.clone();
            let reader = process::spawn_test_thread(move |thread| {
                id.store(u64::from(thread), Ordering::Release);
                reading.read(false, base + 128, 1, false)
            })?;
            while !reader.blocked() {
                crate::time::sleep(1_000_000);
            }
            process::send_thread(
                None,
                tid.load(Ordering::Acquire) as u32,
                Some(SigInfo::new(Signal::USR1, 0)),
                SenderClass::SiUser,
            )
            .map_err(|_| "interrupt reader")?;
            kassert_eq!(reader.join()?, Err(ERESTARTSYS));
            // An idle input thread is also woken and joined before returning ownership.
            drop(session);
            kassert!(!tty.active.load(Ordering::Acquire));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn console_vt_key_encoding_and_processed_output() {
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            kassert!(process::VtId::new(crate::term::VTS).is_none());
            kassert!(process::VtId::new(usize::MAX).is_none());
            let vt = process::VtId::new(1).ok_or("VT id")?;
            let io = ConsoleIo::Vt(vt);
            let _first = crate::term::ShowFirstAfter;
            crate::term::show(vt.index()).map_err(|_| "show VT")?;
            let _attached = crate::vt::Attached::all();
            let tty = console(io).ok_or("VT tty")?;
            let _restore = RestoreConsole::raw(&tty);
            let session = ConsoleSession::prepare(io)?.ok_or("VT owner")?;
            {
                let mut state = tty.lock.lock();
                let mut canonical = Termios::console_default();
                canonical.c_lflag &= !(general::ECHO | general::ECHONL);
                let _ = state.ld.set_termios(canonical, Flush::Input);
            }
            session.activate();
            let base = process::KTEST_USER_VA;
            let keys = |scancodes: &[u8]| -> crate::ktest::TestResult {
                kassert!(crate::keyboard::queue_for_test(scancodes));
                crate::keyboard::wake_readers();
                Ok(())
            };
            // Actual set-1 bytes go through the decoder, VT queue and input thread:
            // a, b, physical Backspace, c, physical Enter (presses and releases).
            keys(&[0x1e, 0x9e, 0x30, 0xb0, 0x0e, 0x8e, 0x2e, 0xae, 0x1c, 0x9c])?;
            kassert_eq!(tty.read(false, base, 16, false), Ok(3));
            let mut input = [0; 3];
            copy_from_user(&mut input, base).map_err(|_| "VT canonical read")?;
            kassert_eq!(&input, b"ac\n");

            {
                let mut state = tty.lock.lock();
                let mut raw = state.ld.termios();
                raw.c_iflag = 0;
                raw.c_lflag = 0;
                raw.c_oflag = general::ONLCR; // OPOST is clear; LF must remain LF.
                let _ = state.ld.set_termios(raw, Flush::Input);
            }
            for (scancodes, expected) in [
                (&[0x1c, 0x9c][..], b'\r'),             // Enter
                (&[0x1d, 0x24, 0xa4, 0x9d][..], b'\n'), // Ctrl-J
                (&[0x0e, 0x8e][..], 0x7f),              // Backspace
                (&[0x1d, 0x23, 0xa3, 0x9d][..], 0x08),  // Ctrl-H
                (&[0xe0, 0x1c, 0xe0, 0x9c][..], b'\r'), // keypad Enter
                (&[0x1d, 0x32, 0xb2, 0x9d][..], b'\r'), // Ctrl-M
            ] {
                keys(scancodes)?;
                kassert_eq!(tty.read(false, base, 1, false), Ok(1));
                copy_from_user(&mut input[..1], base).map_err(|_| "VT raw scancode read")?;
                kassert_eq!(input[0], expected);
            }
            {
                let mut state = tty.lock.lock();
                let mut raw = state.ld.termios();
                raw.c_iflag = general::IGNCR;
                raw.c_cc[general::VMIN as usize] = 2;
                let _ = state.ld.set_termios(raw, Flush::Input);
            }
            // IGNCR drops physical Enter, but Ctrl-J must survive before q.
            keys(&[0x1c, 0x9c, 0x1d, 0x24, 0xa4, 0x9d, 0x10, 0x90])?;
            kassert_eq!(tty.read(false, base, 2, false), Ok(2));
            copy_from_user(&mut input[..2], base).map_err(|_| "VT IGNCR read")?;
            kassert_eq!(&input[..2], b"\nq");

            crate::term::reset_view(vt.index());
            const TEXT: &[u8] = b"\x1b[2J\x1b[Hab\nX";
            copy_to_user(base, TEXT).map_err(|_| "VT output")?;
            let renders = crate::term::tty_renders(vt.index());
            kassert_eq!(tty.write(false, base, TEXT.len() as u64, false), Ok(TEXT.len() as u64));
            kassert_eq!(crate::term::tty_renders(vt.index()) - renders, 1);
            let view = crate::term::view_text().ok_or("VT grid")?;
            kassert_eq!(view.rows[0].as_str(), "ab");
            kassert_eq!(view.rows[1].as_str(), "  X");
            // Both UTF-8 and the cursor-position escape sequence span writes.
            for bytes in [&b"\x1b[3;"[..], &b"1H\xc3"[..], &b"\xa9"[..]] {
                copy_to_user(base, bytes).map_err(|_| "VT split output")?;
                kassert_eq!(tty.write(false, base, bytes.len() as u64, false), Ok(bytes.len() as u64));
            }
            kassert_eq!(crate::term::view_text().ok_or("VT grid")?.rows[2].as_str(), "é");
            drop(session);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn console_vt_output_batches_renders() {
        use crate::kassert_eq;
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let vt = process::VtId::new(1).ok_or("VT id")?;
            let _first = crate::term::ShowFirstAfter;
            crate::term::show(vt.index()).map_err(|_| "show VT")?;
            let tty = console(ConsoleIo::Vt(vt)).ok_or("VT tty")?;
            let _restore = RestoreConsole::raw(&tty);
            let base = process::KTEST_USER_VA;
            for len in [VT_OUTPUT_BATCH - 1, VT_OUTPUT_BATCH, VT_OUTPUT_BATCH + 1, 4 * VT_OUTPUT_BATCH + 1] {
                crate::term::write_str_to(vt.index(), "\x1b[2J\x1b[H");
                let mut bytes = [b'\r'; 4 * VT_OUTPUT_BATCH + 1];
                bytes[len - 1] = b'X';
                copy_to_user(base, &bytes[..len]).map_err(|_| "VT batch output")?;
                let renders = crate::term::tty_renders(vt.index());
                kassert_eq!(tty.write(false, base, len as u64, false), Ok(len as u64));
                kassert_eq!(crate::term::tty_renders(vt.index()) - renders, len.div_ceil(VT_OUTPUT_BATCH) as u64);
                // The final partial batch must be visible when write returns.
                kassert_eq!(crate::term::view_text().ok_or("VT grid")?.rows[0].as_str(), "X");
            }
            {
                let mut state = tty.lock.lock();
                let mut termios = state.ld.termios();
                termios.c_oflag = general::OPOST | general::ONLCR;
                let _ = state.ld.set_termios(termios, Flush::Keep);
            }
            // CRLF expansion straddles a batch boundary: count processed bytes,
            // not source bytes, and preserve the parser's state across batches.
            crate::term::write_str_to(vt.index(), "\x1b[2J\x1b[H");
            let mut bytes = [b'\r'; VT_OUTPUT_BATCH + 1];
            bytes[VT_OUTPUT_BATCH - 1] = b'\n';
            bytes[VT_OUTPUT_BATCH] = b'X';
            copy_to_user(base, &bytes).map_err(|_| "VT expanded batch output")?;
            let renders = crate::term::tty_renders(vt.index());
            kassert_eq!(tty.write(false, base, bytes.len() as u64, false), Ok(bytes.len() as u64));
            kassert_eq!(crate::term::tty_renders(vt.index()) - renders, 2);
            kassert_eq!(crate::term::view_text().ok_or("VT grid")?.rows[1].as_str(), "X");
            Ok(())
        })?
    }
);

crate::ktest!(
    fn console_ctrl_c_flushes_queued_suffix() {
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| -> crate::ktest::TestResult {
            let tty = console(ConsoleIo::Serial).ok_or("serial tty")?;
            let _restore = RestoreConsole::raw(&tty);
            // Ignore delivery to this test process; ISIG must still flush input.
            process::set_current_action(Signal::INT, process::KernelSigaction { handler: 1, ..Default::default() });
            let session = ConsoleSession::prepare(ConsoleIo::Serial)?.ok_or("serial owner")?;
            let (sequence, flush) = {
                let mut state = tty.lock.lock();
                let mut termios = state.ld.termios();
                termios.c_lflag = general::ISIG;
                let _ = state.ld.set_termios(termios, Flush::Input);
                (tty.seq.load(Ordering::Acquire), state.flush)
            };
            // The start gate is closed: prefix, Ctrl-C and suffix are all queued
            // before the input thread can consume any of them, on any host.
            for &byte in b"a\x03suffix" {
                crate::serial_in::inject_for_test(byte);
            }
            session.activate();
            while tty.seq.load(Ordering::Acquire).wrapping_sub(sequence) < 2 {
                crate::time::sleep(1_000_000);
            }
            {
                let state = tty.lock.lock();
                kassert_eq!(state.flush, flush.wrapping_add(1));
                kassert_eq!(state.pending_len, 0);
                kassert_eq!(state.ld.readable_bytes(), 0);
                kassert!(!crate::serial_in::available());
            }
            let base = process::KTEST_USER_VA;
            kassert_eq!(tty.read(false, base, 16, true), Err(errno::EAGAIN));
            crate::serial_in::inject_for_test(b'z');
            kassert_eq!(tty.read(false, base, 1, false), Ok(1));
            let mut byte = [0];
            copy_from_user(&mut byte, base).map_err(|_| "fresh input after Ctrl-C")?;
            kassert_eq!(byte, *b"z");
            drop(session);
            Ok(())
        })?
    }
);
