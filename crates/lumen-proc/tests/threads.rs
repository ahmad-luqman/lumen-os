//! C3's executable interleaving model, before the kernel implementation.
//!
//! Linux reference: fs/exec.c::de_thread/exchange_tids;
//! kernel/exit.c::do_group_exit; kernel/signal.c::zap_other_threads,
//! do_signal_stop, complete_signal, exit_signals, retarget_shared_pending,
//! posixtimer_send_sigqueue; kernel/time/posix-timers.c::posix_timer_delete,
//! posixtimer_exec, __posixtimer_deliver_signal. Each method is one siglock
//! transaction, except finish_exec, which also represents tasklist_lock.
//! Timer rearming at expiry (including ignored/dead targets), immediate
//! stale-signal discard, and instance keys are the design's explicit Linux
//! deviations. C13 implements those timers; this model fixes their contract.

use lumen_proc::{ProcTree, RusageSlot};
use lumen_signal::pending::{PendingSignals, SenderClass, SigInfo, Target, ThreadSlot};
use lumen_signal::{Blocked, Signal};

struct Thread {
    tid: u32,
    identity: u64,
    slot: ThreadSlot,
    stop_dequeued: bool,
    stop_pending: bool,
    fatal: bool,
    notified: bool,
}

struct Group {
    tree: ProcTree,
    pid: u32,
    threads: Vec<Thread>,
    pending: PendingSignals,
    exec: Option<u64>,
    exit: Option<u8>,
    stopping: bool,
    remaining: usize,
    stop_reports: usize,
    next_identity: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    Intr,
    Restart,
    Again,
}

impl Group {
    fn new() -> Self {
        let mut tree = ProcTree::new(64);
        let pid = tree.spawn_root(RusageSlot::new(0)).unwrap();
        let (pending, slots) = PendingSignals::new(1, 16).unwrap();
        let mut group = Self {
            tree,
            pid,
            pending,
            threads: Vec::new(),
            exec: None,
            exit: None,
            stopping: false,
            remaining: 0,
            stop_reports: 0,
            next_identity: 1,
        };
        group.insert(pid, slots[0]);
        group.clone_thread().unwrap();
        group
    }

    fn insert(&mut self, tid: u32, slot: ThreadSlot) {
        let identity = self.next_identity;
        self.next_identity += 1;
        self.threads.push(Thread {
            tid,
            identity,
            slot,
            stop_dequeued: false,
            stop_pending: false,
            fatal: false,
            notified: false,
        });
    }

    fn clone_thread(&mut self) -> Result<u64, Refusal> {
        if self.exit.is_some() {
            return Err(Refusal::Intr);
        }
        if self.exec.is_some() || self.stopping {
            return Err(Refusal::Restart);
        }
        let tid = self.tree.add_thread(self.pid).unwrap();
        let slot = self.pending.add_thread().unwrap();
        self.insert(tid, slot);
        Ok(self.threads.last().unwrap().identity)
    }

    fn cancel_stop(&mut self) {
        self.stopping = false;
        self.remaining = 0;
        for t in &mut self.threads {
            t.stop_dequeued = false;
            t.stop_pending = false;
        }
    }

    fn start_stop(&mut self, identity: u64) {
        if self.exit.is_some()
            || self.exec.is_some()
            || !self.threads.iter().any(|t| t.identity == identity && t.stop_dequeued)
        {
            return;
        }
        self.stopping = true;
        self.remaining = self.threads.len();
        for t in &mut self.threads {
            t.stop_pending = true;
        }
    }

    fn begin_exec(&mut self, identity: u64) -> Result<(), Refusal> {
        if self.exit.is_some() || self.exec.is_some() {
            return Err(Refusal::Again);
        }
        self.exec = Some(identity);
        self.cancel_stop();
        for t in &mut self.threads {
            if t.identity != identity {
                self.pending
                    .enqueue(Target::Thread(t.slot), SigInfo::new(Signal::KILL, 0), SenderClass::SiUser)
                    .unwrap();
                t.fatal = true;
                t.notified = true;
            }
        }
        Ok(())
    }

    fn group_exit(&mut self, identity: u64, code: u8) {
        if self.exit.is_none() && self.exec.is_some_and(|exec| exec != identity) {
            return;
        }
        self.exit.get_or_insert(code);
        self.cancel_stop();
        for t in &mut self.threads {
            t.fatal = true;
            t.notified = true;
        }
    }

    fn fatal_send(&mut self) {
        // complete_signal does not exempt group_exec_task.
        self.exit.get_or_insert(9);
        self.cancel_stop();
        for t in &mut self.threads {
            t.fatal = true;
            t.notified = true;
        }
    }

    fn leave(&mut self, identity: u64) {
        let i = self.threads.iter().position(|t| t.identity == identity).unwrap();
        let t = self.threads.remove(i);
        if t.tid != self.pid {
            self.tree.remove_thread(self.pid, t.tid).unwrap();
        }
        self.pending.remove_thread(t.slot).unwrap();
        if self.exit.is_none() {
            if t.stop_pending {
                self.remaining -= 1;
                if self.remaining == 0 {
                    self.stop_reports += 1;
                }
            }
            for other in &mut self.threads {
                if self.pending.has_deliverable(other.slot, Blocked::EMPTY).unwrap() {
                    other.notified = true;
                }
            }
        }
    }

    fn finish_exec(&mut self) -> Result<(), Refusal> {
        if self.exit.is_some() {
            self.exec = None;
            return Err(Refusal::Again);
        }
        assert_eq!(self.threads.len(), 1);
        let thread = &mut self.threads[0];
        assert_eq!(self.exec, Some(thread.identity));
        if thread.tid != self.pid {
            self.tree.remove_thread(self.pid, thread.tid).unwrap();
            thread.tid = self.pid;
        }
        self.exec = None;
        Ok(())
    }
}

#[test]
fn clone_and_group_exit_both_orders() {
    for exit_first in [false, true] {
        let mut g = Group::new();
        if exit_first {
            g.group_exit(1, 42);
        }
        let cloned = g.clone_thread();
        if !exit_first {
            g.group_exit(1, 42);
        }
        assert_eq!(cloned.is_err(), exit_first);
        assert!(g.threads.iter().all(|t| t.fatal && t.notified));
        assert_eq!(g.exit, Some(42));
    }
    let mut g = Group::new();
    g.threads[0].stop_dequeued = true;
    g.start_stop(1);
    assert_eq!(g.clone_thread(), Err(Refusal::Restart));
    g.cancel_stop();
    g.begin_exec(2).unwrap();
    assert_eq!(g.clone_thread(), Err(Refusal::Restart));
    assert_eq!(g.begin_exec(1), Err(Refusal::Again));
}

#[test]
fn stop_gap_exec_and_continue_all_orders() {
    let mut g = Group::new();
    g.threads[0].stop_dequeued = true;
    g.exec = Some(2); // isolate the exec guard from cancel_stop's mark clearing
    g.start_stop(1);
    assert!(!g.stopping);
    for first in 0..3 {
        let mut g = Group::new();
        g.threads[0].stop_dequeued = true;
        match first {
            0 => {
                g.begin_exec(2).unwrap();
                g.start_stop(1);
            }
            1 => {
                g.start_stop(1);
                g.begin_exec(2).unwrap();
            }
            _ => {
                g.cancel_stop();
                g.start_stop(1);
            }
        }
        assert!(!g.stopping);
        assert_eq!(g.remaining, 0);
        assert!(g.threads.iter().all(|t| !t.stop_pending && !t.stop_dequeued));
    }
}

#[test]
fn sibling_group_exit_and_fatal_dequeue_do_not_kill_exec() {
    for code in [0, 9, 42] {
        let mut g = Group::new();
        let old_tid = g.threads[1].tid;
        g.begin_exec(2).unwrap();
        assert!(g.threads[0].fatal); // wakes TASK_KILLABLE, including vfork
        g.group_exit(1, code); // do_group_exit's locked recheck
        assert_eq!(g.exit, None);
        g.leave(1);
        g.finish_exec().unwrap();
        assert_eq!(g.threads[0].tid, g.pid);
        assert!(!g.tree.is_busy(old_tid));
        assert!(!g.threads[0].fatal);
        assert!(g.tree.threads(g.pid).is_empty());
    }
}

#[test]
fn fatal_send_ends_exec_before_or_during_wait() {
    for before in [false, true] {
        let mut g = Group::new();
        if before {
            g.fatal_send();
        }
        let started = g.begin_exec(2);
        if !before {
            g.fatal_send();
        }
        assert_eq!(started.is_err(), before);
        assert!(g.threads.iter().all(|t| t.fatal));
        assert_eq!(g.finish_exec(), Err(Refusal::Again));
        assert_eq!(g.exec, None);
    }
}

#[test]
fn exit_retargets_shared_queue_and_counts_last_stop_participant() {
    for exec in [false, true] {
        let mut g = Group::new();
        g.pending.enqueue(Target::Process, SigInfo::new(Signal::USR1, 0), SenderClass::SiUser).unwrap();
        g.threads[0].notified = true;
        if exec {
            g.begin_exec(2).unwrap();
        }
        g.leave(1);
        if exec {
            g.finish_exec().unwrap();
        }
        assert!(g.threads[0].notified);
        let slot = g.threads[0].slot;
        assert_eq!(g.pending.dequeue(slot, Blocked::EMPTY).unwrap().unwrap().signal(), Signal::USR1);
        assert_eq!(g.pending.dequeue(slot, Blocked::EMPTY).unwrap(), None);
    }
    let mut g = Group::new();
    g.threads[0].stop_dequeued = true;
    g.start_stop(1);
    g.threads[0].stop_pending = false;
    g.remaining -= 1; // first participant parks
    g.leave(2);
    assert_eq!((g.remaining, g.stop_reports), (0, 1));
}

#[derive(Clone, Copy)]
enum Notify {
    Process,
    Thread(u64),
    None,
}

struct Timer {
    instance: Option<u64>,
    next: u64,
    queued: bool,
    periodic: bool,
    arms: usize,
    sends: usize,
    overrun: usize,
    notify: Notify,
    real: bool,
}
impl Timer {
    fn new(notify: Notify, real: bool) -> Self {
        Self { instance: Some(1), next: 2, queued: false, periodic: true, arms: 1, sends: 0, overrun: 0, notify, real }
    }
    fn replace(&mut self, armed: bool) {
        self.instance = armed.then_some(self.next);
        self.next += 1;
        self.queued = false;
        self.overrun = 0;
    }
    fn exec(&mut self) {
        if !self.real {
            self.replace(false);
        }
    }
    fn expire(&mut self, captured: u64, group: &Group, ignored: bool, full: bool) {
        if self.instance != Some(captured) {
            return;
        }
        if matches!(self.notify, Notify::None) {
            return;
        }
        let alive = match self.notify {
            Notify::Thread(id) => group.threads.iter().any(|t| t.identity == id),
            _ => true,
        };
        if !self.real && self.queued {
            self.overrun += 1;
        } else if alive && !ignored && group.exit.is_none() {
            if full {
                self.overrun += 1;
            } else {
                self.sends += 1;
                self.queued = true;
            }
        }
        if self.periodic {
            self.arms += 1;
        }
    }

    /// Instance check, ordinary send effects, and periodic rearm are one
    /// transaction. Even an ignored CONT cancels the stop before being dropped.
    fn expire_signal(&mut self, captured: u64, group: &mut Group, signal: Signal, ignored: bool, real_blocked: bool) {
        let alive = match self.notify {
            Notify::Process => true,
            Notify::Thread(id) => group.threads.iter().any(|t| t.identity == id),
            Notify::None => false,
        };
        let send = self.instance == Some(captured) && !self.queued && alive && group.exit.is_none();
        if send && signal == Signal::CONT {
            group.cancel_stop();
        }
        self.expire(captured, group, ignored && !real_blocked, false);
        if !send || (ignored && !real_blocked) {
            return;
        }
        if signal == Signal::KILL || (signal == Signal::new(32).unwrap() && !real_blocked) {
            group.fatal_send();
        } else if signal == Signal::STOP {
            group.threads[0].stop_dequeued = true;
            group.start_stop(group.threads[0].identity);
        } else if real_blocked {
            let Notify::Thread(id) = self.notify else { panic!("reserved-signal helper is thread-directed") };
            let slot = group.threads.iter().find(|t| t.identity == id).unwrap().slot;
            group.pending.enqueue(Target::Thread(slot), SigInfo::new(signal, 0), SenderClass::Other).unwrap();
        }
    }
}

#[test]
fn timer_expiry_delete_rearm_exec_and_identity_orders() {
    for change in 0..4 {
        for expiry_first in [false, true] {
            let g = Group::new();
            let mut t = Timer::new(Notify::Process, false);
            if expiry_first {
                t.expire(1, &g, false, false);
            }
            match change {
                0 => t.replace(false),
                1 => t.replace(true),
                2 => t.exec(),
                _ => {
                    t.replace(false);
                    t.replace(true);
                }
            }
            if !expiry_first {
                t.expire(1, &g, false, false);
            }
            assert_eq!(t.sends, usize::from(expiry_first));
            assert_eq!(t.arms, 1 + usize::from(expiry_first));
            assert!(!t.queued);
        }
    }
    let mut g = Group::new();
    let old_tid = g.threads[0].tid;
    let survivor_id = g.clone_thread().unwrap();
    let mut dead = Timer::new(Notify::Thread(1), false);
    let mut survivor = Timer::new(Notify::Thread(survivor_id), false);
    g.begin_exec(survivor_id).unwrap();
    g.leave(1);
    g.leave(2);
    g.finish_exec().unwrap();
    assert_eq!(g.threads[0].tid, old_tid);
    assert_ne!(u64::from(old_tid), survivor_id); // numeric tid cannot stand in for identity
    dead.expire(1, &g, false, false);
    survivor.expire(1, &g, false, false);
    assert_eq!((dead.sends, dead.arms), (0, 2));
    assert_eq!(survivor.sends, 1); // identity, not the exchanged numeric tid
    for real in [false, true] {
        let mut t = Timer::new(Notify::Process, real);
        t.exec();
        t.expire(1, &g, false, false);
        assert_eq!(t.sends, usize::from(real)); // ITIMER_REAL survives exec
    }
}

#[test]
fn timer_rearms_without_delivery_and_queue_controls_overruns() {
    let g = Group::new();
    for (notify, ignored, full) in
        [(Notify::Thread(99), false, false), (Notify::Process, true, false), (Notify::Process, false, true)]
    {
        let mut t = Timer::new(notify, false);
        t.expire(1, &g, ignored, full);
        assert_eq!((t.sends, t.arms), (0, 2));
        assert_eq!(t.overrun, usize::from(full));
    }
    let mut t = Timer::new(Notify::Process, false);
    t.expire(1, &g, false, false);
    t.expire(1, &g, false, false);
    assert_eq!((t.sends, t.overrun, t.arms), (1, 1, 3));
    t.queued = false; // disposition change, dequeue, or explicit discard
    t.expire(1, &g, false, false);
    assert_eq!((t.sends, t.arms), (2, 4));
    let mut none = Timer::new(Notify::None, false);
    none.expire(1, &g, false, false);
    assert_eq!((none.sends, none.arms), (0, 1));
}

#[test]
fn timer_send_effects_serialize_with_exec_and_delete() {
    for deleted in [false, true] {
        let mut g = Group::new();
        let mut timer = Timer::new(Notify::Process, false);
        assert!(!g.stopping);
        if deleted {
            timer.replace(false);
        }
        timer.expire_signal(1, &mut g, Signal::STOP, false, false);
        assert_eq!(g.stopping, !deleted, "a live SIGSTOP expiry must stop a running group");
        assert_eq!(g.remaining, if deleted { 0 } else { g.threads.len() });
        assert!(g.threads.iter().all(|t| t.stop_pending == !deleted));
        g.begin_exec(2).unwrap();
        assert!(!g.stopping);
    }
    for signal in [Signal::CONT, Signal::STOP, Signal::KILL] {
        for deleted in [false, true] {
            for exec_first in [false, true] {
                let mut g = Group::new();
                let mut timer = Timer::new(Notify::Process, false);
                g.threads[0].stop_dequeued = true;
                g.start_stop(1);
                if exec_first {
                    g.begin_exec(2).unwrap();
                }
                if deleted {
                    timer.replace(false);
                }
                timer.expire_signal(1, &mut g, signal, signal == Signal::CONT, false);
                assert_eq!(timer.arms, if deleted { 1 } else { 2 });
                assert_eq!(g.exit, (!deleted && signal == Signal::KILL).then_some(9));
                if !deleted && signal == Signal::CONT {
                    assert!(!g.stopping); // SIG_IGN still has the continue effect
                }
                if !exec_first {
                    assert_eq!(g.begin_exec(2).is_err(), !deleted && signal == Signal::KILL);
                }
                assert!(!g.stopping); // late STOP cannot outlive de_thread
            }
        }
    }
}

#[test]
fn timer_helper_real_mask_preserves_reserved_signal_without_group_kill() {
    let signal = Signal::new(32).unwrap();
    for real_blocked in [false, true] {
        let mut g = Group::new();
        let mut timer = Timer::new(Notify::Thread(2), false);
        timer.expire_signal(1, &mut g, signal, false, real_blocked);
        assert_eq!(g.exit.is_some(), !real_blocked);
        if real_blocked {
            let slot = g.threads[1].slot;
            assert_eq!(g.pending.dequeue(slot, Blocked::EMPTY).unwrap().unwrap().signal(), signal);
            assert_eq!(g.pending.dequeue(slot, Blocked::EMPTY).unwrap(), None);
            g.begin_exec(1).unwrap();
            g.leave(2);
            timer.queued = false;
            timer.expire_signal(1, &mut g, signal, false, true);
            assert_eq!(timer.sends, 1); // dead helper identity cannot target the leader
            assert_eq!(timer.arms, 3);
            assert_eq!(g.exit, None);
        }
    }
    let mut g = Group::new();
    let mut timer = Timer::new(Notify::Thread(2), false);
    timer.expire_signal(1, &mut g, signal, true, true);
    assert_eq!(timer.sends, 1); // real_blocked also exempts the ignored-drop path
}
