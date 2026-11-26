//! Process timers. Expiry, queue inspection, send and rearm share PROCS then siglock.

use super::{PROCS, Process, SignalState, UserThread};
use crate::linux::{errno, general};
use crate::{sched, sync::SpinLock, time};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use lumen_signal::Signal;
use lumen_signal::pending::{Enqueued, PendingSignal, SigInfo};
use lumen_signal::timer::Setting;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerId {
    Real,
    Posix(i32),
}

pub enum TimerNotify {
    Process,
    Thread(Weak<UserThread>),
    None,
}

pub struct PosixTimer {
    pub clock: i32,
    pub notify: TimerNotify,
    signal: Signal,
    sival: u64,
    setting: Setting,
    instance: Option<u64>,
    birth: u64,
}

impl PosixTimer {
    fn new(clock: i32, notify: TimerNotify, signal: Signal, sival: u64) -> Self {
        Self { clock, notify, signal, sival, setting: Setting::default(), instance: None, birth: 0 }
    }
}

pub struct Timers {
    real: PosixTimer,
    posix: BTreeMap<i32, PosixTimer>,
    next_instance: u64,
    owner: Option<(u32, Weak<Process>)>,
    #[cfg(feature = "ktest")]
    test_now: Option<u64>,
}
impl Default for Timers {
    fn default() -> Self {
        Self {
            real: PosixTimer::new(1, TimerNotify::Process, Signal::ALRM, 0),
            posix: BTreeMap::new(),
            next_instance: 0,
            owner: None,
            #[cfg(feature = "ktest")]
            test_now: None,
        }
    }
}

impl Timers {
    pub fn get(&self, id: TimerId) -> Result<&PosixTimer, u32> {
        match id {
            TimerId::Real => Ok(&self.real),
            TimerId::Posix(id) => self.posix.get(&id).ok_or(errno::EINVAL),
        }
    }
    fn get_mut(&mut self, id: TimerId) -> Result<&mut PosixTimer, u32> {
        match id {
            TimerId::Real => Ok(&mut self.real),
            TimerId::Posix(id) => self.posix.get_mut(&id).ok_or(errno::EINVAL),
        }
    }
    fn now(&self) -> u64 {
        #[cfg(feature = "ktest")]
        if let Some(now) = self.test_now {
            return now;
        }
        time::now_ns()
    }
    /// Account a consumed notification before either sigwait or a handler sees it.
    pub fn delivered(&mut self, entry: PendingSignal) -> SigInfo {
        let mut info = entry.info();
        let Some(instance) = entry.timer_instance() else { return info };
        let now = self.now();
        let Some((&id, timer)) = self.posix.iter_mut().find(|(_, t)| t.instance == Some(instance)) else { return info };
        let (pid, owner) = self.owner.as_ref().expect("armed timer owner");
        remove_entry(*pid, timer);
        let count = timer.setting.forward(now);
        timer.setting.missed(count);
        info.set_timer_overrun(timer.setting.delivered());
        insert_entry_for(*pid, owner, TimerId::Posix(id), timer);
        info
    }
    /// Retire global entries before exit can release the pid. Exec retains REAL.
    pub fn clear(&mut self, pid: u32, exiting: bool) {
        for timer in self.posix.values() {
            remove_entry(pid, timer);
        }
        self.posix.clear();
        if exiting {
            remove_entry(pid, &self.real);
            self.real.setting = Setting::default();
            self.real.instance = None;
        }
    }
}

type Key = (u64, u32, u64);
type Entry = (Weak<Process>, TimerId);
static TIMERS: SpinLock<BTreeMap<Key, Entry>> = SpinLock::new(BTreeMap::new());
static EARLIEST: AtomicU64 = AtomicU64::new(u64::MAX);
static WAKE: sched::WaitQueue = sched::WaitQueue::new();

fn publish(map: &BTreeMap<Key, Entry>) {
    EARLIEST.store(map.first_key_value().map_or(u64::MAX, |(key, _)| key.0), Ordering::Release);
}
fn remove_entry(pid: u32, timer: &PosixTimer) {
    if let (Some(deadline), Some(instance)) = (timer.setting.deadline(), timer.instance) {
        let mut map = TIMERS.lock();
        map.remove(&(deadline, pid, instance));
        publish(&map);
    }
}
fn insert_entry(process: &Arc<Process>, id: TimerId, timer: &PosixTimer) {
    insert_entry_for(process.pid(), &Arc::downgrade(process), id, timer);
}
fn insert_entry_for(pid: u32, owner: &Weak<Process>, id: TimerId, timer: &PosixTimer) {
    if !matches!(timer.notify, TimerNotify::None)
        && let (Some(deadline), Some(instance)) = (timer.setting.deadline(), timer.instance)
    {
        let mut map = TIMERS.lock();
        map.insert((deadline, pid, instance), (owner.clone(), id));
        publish(&map);
    }
}

/// Boot CPU tick: one deadline load while idle; signal generation stays in thread context.
pub fn on_tick() {
    let deadline = EARLIEST.load(Ordering::Acquire);
    if deadline != u64::MAX && time::now_ns() >= deadline {
        WAKE.wake_all();
    }
}

#[cfg(feature = "ktest")]
static WORKER_GATE: crate::sync::SleepMutex<()> = crate::sync::SleepMutex::new(());

pub fn init() {
    let _ = sched::spawn("timers", || {
        loop {
            WAKE.wait_until(|| {
                let deadline = EARLIEST.load(Ordering::Acquire);
                deadline != u64::MAX && time::now_ns() >= deadline
            });
            #[cfg(feature = "ktest")]
            let _worker = WORKER_GATE.lock();
            let now = time::now_ns();
            let expired: Vec<_> = {
                let mut map = TIMERS.lock();
                let mut expired = Vec::new();
                while map.first_key_value().is_some_and(|(key, _)| key.0 <= now) {
                    expired.push(map.pop_first().unwrap());
                }
                publish(&map);
                expired
            };
            for ((_, _, instance), (owner, id)) in expired {
                if let Some(process) = owner.upgrade() {
                    PROCS.lock().expire_timer(&process, id, instance, now);
                    // The final Process reference is dropped outside all locks.
                }
            }
        }
    })
    .expect("timers thread");
}

/// A creation receipt prevents failed copyout from deleting a reused numeric id.
pub struct Created {
    pub id: i32,
    birth: u64,
}

pub fn cancel_create(process: &Process, created: Created) {
    let mut state = process.signal.lock();
    if state.timers.posix.get(&created.id).is_some_and(|t| t.birth == created.birth) {
        let timer = state.timers.posix.remove(&created.id).unwrap();
        remove_entry(process.pid(), &timer);
        if let Some(instance) = timer.instance {
            state.pending.discard_timer(instance);
        }
    }
}

/// Validate a SIGEV_THREAD_ID under the same table hold as creation.
pub fn create(
    process: &Arc<Process>,
    clock: i32,
    mode: i32,
    signal: u32,
    value: Option<u64>,
    tid: u32,
) -> Result<Created, u32> {
    if !matches!(clock, 0 | 1 | 7) {
        return Err(errno::EINVAL);
    }
    let table = PROCS.lock();
    let notify = match mode {
        0 | 2 => TimerNotify::Process,
        1 => TimerNotify::None,
        4 => {
            let target = table.lookup_thread(tid).ok_or(errno::EINVAL)?;
            if !Arc::ptr_eq(target.process(), process) {
                return Err(errno::EINVAL);
            }
            TimerNotify::Thread(Arc::downgrade(&target))
        }
        _ => return Err(errno::EINVAL),
    };
    // Linux ignores the unused signo for SIGEV_NONE.
    let signal = if mode == 1 { Signal::ALRM } else { Signal::new(signal).ok_or(errno::EINVAL)? };
    let mut state = process.signal.lock();
    let id = (0..32).find(|id| !state.timers.posix.contains_key(id)).ok_or(errno::EAGAIN)?;
    let birth = state.timers.next_instance.checked_add(1).ok_or(errno::EAGAIN)?;
    state.timers.next_instance = birth;
    let mut timer = PosixTimer::new(clock, notify, signal, value.unwrap_or(id as u64));
    timer.birth = birth;
    state.timers.owner = Some((process.pid(), Arc::downgrade(process)));
    state.timers.posix.insert(id, timer);
    Ok(Created { id, birth })
}

/// Snapshot time left and interval, preserving pending-notification accounting.
pub fn get(process: &Process, id: TimerId) -> Result<(u64, u64), u32> {
    let mut state = process.signal.lock();
    let now = state.timers.now();
    let pending = notification_pending(&state, id)?;
    Ok(snapshot(state.timers.get_mut(id)?, pending, now))
}
fn notification_pending(state: &SignalState, id: TimerId) -> Result<bool, u32> {
    Ok(matches!(id, TimerId::Posix(_))
        && state.timers.get(id)?.instance.is_some_and(|instance| state.pending.any_timer(instance)))
}
fn snapshot(timer: &mut PosixTimer, pending: bool, now: u64) -> (u64, u64) {
    if matches!(timer.notify, TimerNotify::None) {
        let count = timer.setting.forward(now);
        timer.setting.missed(count);
    }
    // common_timer_get forwards a periodic timer while its notification is
    // pending. Project the remainder without moving the live deadline: the
    // worker or delivery still accounts every period exactly once.
    let mut reported = timer.setting;
    if pending && reported.interval() != 0 {
        reported.forward(now);
    }
    (reported.interval(), reported.remaining(now))
}

/// Replace a setting and invalidate captured expiries. Only POSIX timers cancel
/// their queued notifications; a pending ITIMER_REAL SIGALRM survives disarming.
pub fn set(process: &Arc<Process>, id: TimerId, interval: u64, value: u64, absolute: bool) -> Result<(u64, u64), u32> {
    let mut state = process.signal.lock();
    let clock = state.timers.get(id)?.clock;
    let now = state.timers.now();
    let pending = notification_pending(&state, id)?;
    let setting = if absolute && clock == 0 {
        let (sec, ns) = time::realtime_sec_nsec();
        let real = sec.saturating_mul(1_000_000_000).saturating_add(ns);
        Setting::realtime(value, interval, real, time::now_ns())
    } else {
        let deadline = (value != 0).then(|| if absolute { value } else { now.saturating_add(value) });
        Setting::new(deadline, interval)
    };
    let instance = if setting.deadline().is_some() {
        let next = state.timers.next_instance.checked_add(1).ok_or(errno::EAGAIN)?;
        state.timers.next_instance = next;
        Some(next)
    } else {
        None
    };
    if matches!(id, TimerId::Posix(_))
        && let Some(previous) = state.timers.get(id)?.instance
    {
        state.pending.discard_timer(previous);
    }
    state.timers.owner = Some((process.pid(), Arc::downgrade(process)));
    let timer = state.timers.get_mut(id)?;
    remove_entry(process.pid(), timer);
    let old = snapshot(timer, pending, now);
    timer.setting = setting;
    timer.instance = instance;
    insert_entry(process, id, timer);
    Ok(old)
}

pub fn delete(process: &Process, id: i32) -> Result<(), u32> {
    let mut state = process.signal.lock();
    let timer = state.timers.posix.remove(&id).ok_or(errno::EINVAL)?;
    remove_entry(process.pid(), &timer);
    if let Some(instance) = timer.instance {
        state.pending.discard_timer(instance);
    }
    Ok(())
}
pub fn overrun(process: &Process, id: i32) -> Result<i32, u32> {
    Ok(process.signal.lock().timers.get(TimerId::Posix(id))?.setting.overrun_last())
}

/// None means stale; a live entry may still need rearming without a send.
pub(super) fn prepare(state: &mut SignalState, id: TimerId, instance: u64, now: u64) -> Option<Option<SigInfo>> {
    let timer = state.timers.get_mut(id).ok()?;
    if timer.instance != Some(instance) || !timer.setting.deadline().is_some_and(|deadline| deadline <= now) {
        return None;
    }
    let info = match id {
        TimerId::Real => SigInfo::new(Signal::ALRM, general::SI_KERNEL as i32),
        TimerId::Posix(id) => SigInfo::timer(timer.signal, id, timer.sival),
    };
    if let TimerId::Posix(id) = id
        && state.pending.any_timer(instance)
    {
        state.timers.get_mut(TimerId::Posix(id)).unwrap().setting.missed(1);
        return Some(None);
    }
    Some(Some(info))
}

pub(super) fn finish(
    process: &Arc<Process>,
    state: &mut SignalState,
    id: TimerId,
    now: u64,
    result: Result<Option<Enqueued>, u32>,
) {
    let timer = state.timers.get_mut(id).expect("expiry holds siglock");
    remove_entry(process.pid(), timer);
    let count = timer.setting.forward(now);
    if matches!(id, TimerId::Posix(_)) {
        let refused = matches!(result, Ok(Some(Enqueued::Coalesced)) | Err(_));
        timer.setting.missed(count.saturating_sub(1));
        if refused {
            timer.setting.enqueue_refused();
        }
    }
    insert_entry(process, id, timer);
}

crate::ktest!(
    fn timer_instances_queues_and_overruns() {
        use super::{current_process, current_thread, in_test_process};
        use crate::{kassert, kassert_eq};
        use lumen_signal::{Blocked, SigSet};
        in_test_process(|_| {
            let process = current_process();
            let me = current_thread();
            let wanted = SigSet::of(&[Signal::RTMIN]);
            me.set_blocked(Blocked::new(wanted));
            let id = create(&process, 1, 0, 32, Some(123), 0).map_err(|_| "create")?.id;
            let timer_id = TimerId::Posix(id);
            // Far-future supplied times keep the background worker out of this
            // deterministic test; expiry uses precisely the production path.
            let base = time::now_ns() + 1_000_000_000_000;
            set(&process, timer_id, 10, base, true).map_err(|_| "arm")?;
            let instance = process.signal.lock().timers.get(timer_id).unwrap().instance.unwrap();
            PROCS.lock().expire_timer(&process, timer_id, instance, base + 35);
            kassert_eq!(overrun(&process, id), Ok(0));
            let info = me.wait_signal(wanted, Some(time::now_ns())).map_err(|_| "queued expiry")?;
            kassert_eq!(info.timer_id(), Some(id));
            kassert_eq!(i32::from_le_bytes(info.bytes()[20..24].try_into().unwrap()), 3);
            kassert_eq!(overrun(&process, id), Ok(3));
            PROCS.lock().expire_timer(&process, timer_id, instance, base + 40);
            PROCS.lock().expire_timer(&process, timer_id, instance, base + 50);
            kassert_eq!(overrun(&process, id), Ok(3));
            let info = me.wait_signal(wanted, Some(time::now_ns())).map_err(|_| "periodic expiry")?;
            kassert_eq!(i32::from_le_bytes(info.bytes()[20..24].try_into().unwrap()), 1);
            // A replacement removes a pending notification and captured work.
            PROCS.lock().expire_timer(&process, timer_id, instance, base + 60);
            set(&process, timer_id, 0, base + 100, true).map_err(|_| "replace")?;
            PROCS.lock().expire_timer(&process, timer_id, instance, base + 1000);
            kassert!(!process.signal.lock().pending.any(|i| i.timer_id() == Some(id)));
            kassert_eq!(overrun(&process, id), Ok(0));
            delete(&process, id).map_err(|_| "delete")?;
            let reused = create(&process, 1, 0, 32, None, 0).map_err(|_| "reuse")?.id;
            kassert_eq!(reused, id);
            set(&process, timer_id, 0, base + 200, true).map_err(|_| "new arm")?;
            PROCS.lock().expire_timer(&process, timer_id, instance, base + 1000);
            kassert!(!process.signal.lock().pending.any(|_| true));
            delete(&process, id).map_err(|_| "cleanup")?;
            kassert!(!TIMERS.lock().keys().any(|key| key.1 == process.pid()));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn timer_dead_identity_exec_and_exit_cleanup() {
        use super::{add_ktest_thread, current_process, current_thread, in_test_process, remove_ktest_thread};
        use crate::{kassert, kassert_eq};
        use lumen_signal::{Blocked, SigSet};
        in_test_process(|_| {
            let process = current_process();
            current_thread().set_blocked(Blocked::new(SigSet::of(&[Signal::RTMIN, Signal::ALRM])));
            let target = add_ktest_thread(&process)?;
            let id = create(&process, 1, 4, 32, None, target.tid()).map_err(|_| "thread timer")?.id;
            let base = time::now_ns() + 1_000_000_000_000;
            set(&process, TimerId::Posix(id), 10, base, true).map_err(|_| "arm")?;
            let instance = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().instance.unwrap();
            remove_ktest_thread(&target);
            let replacement = add_ktest_thread(&process)?;
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base);
            let pending = process.signal.lock().pending.any(|_| true);
            let deadline = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().setting.deadline();
            remove_ktest_thread(&replacement);
            kassert!(!pending);
            kassert_eq!(deadline, Some(base + 10));
            set(&process, TimerId::Real, 0, base, true).map_err(|_| "real")?;
            process.exec_signals(current_thread().tid());
            kassert_eq!(overrun(&process, id), Err(errno::EINVAL));
            kassert!(process.signal.lock().timers.real.instance.is_some());
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base + 100);
            kassert!(!process.signal.lock().pending.any(|_| true));
            // Exit's production retirement removes every key before pid reuse.
            process.signal.lock().timers.clear(process.pid(), true);
            kassert!(!TIMERS.lock().keys().any(|key| key.1 == process.pid()));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn timer_send_effects_and_creation_rollback() {
        use super::{current_process, current_thread, in_test_process, send_process_checked, set_current_action};
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::SenderClass;
        use lumen_signal::{Blocked, SigSet};
        in_test_process(|pid| {
            let process = current_process();
            let me = current_thread();
            me.set_blocked(Blocked::new(SigSet::of(&[Signal::ALRM, Signal::RTMIN])));
            let receipt = create(&process, 1, 0, 14, None, 0).map_err(|_| "create")?;
            let id = receipt.id;
            delete(&process, id).map_err(|_| "delete")?;
            let new = create(&process, 1, 0, 14, None, 0).map_err(|_| "reuse")?;
            kassert_eq!(new.id, id);
            cancel_create(&process, receipt);
            kassert_eq!(overrun(&process, id), Ok(0));
            let base = time::now_ns() + 1_000_000_000_000;
            set(&process, TimerId::Posix(id), 10, base, true).map_err(|_| "arm")?;
            let instance = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().instance.unwrap();
            send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::ALRM, 0))), SenderClass::SiUser)
                .map_err(|_| "other sender")?;
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base);
            let wanted = SigSet::of(&[Signal::ALRM]);
            let ordinary = me.wait_signal(wanted, Some(time::now_ns())).map_err(|_| "ordinary signal")?;
            kassert_eq!(ordinary.timer_id(), None);
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base + 10);
            let timer_info = me.wait_signal(wanted, Some(time::now_ns())).map_err(|_| "timer signal")?;
            kassert_eq!(timer_info.timer_id(), Some(id));
            kassert_eq!(overrun(&process, id), Ok(1));
            delete(&process, id).map_err(|_| "delete")?;
            // A full RT queue is a refused enqueue, not a queued timer. Its
            // overrun is reported with the next successfully delivered expiry.
            let id = create(&process, 1, 0, 32, None, 0).map_err(|_| "RT timer")?.id;
            set(&process, TimerId::Posix(id), 10, base, true).map_err(|_| "arm RT")?;
            let instance = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().instance.unwrap();
            let fill =
                || send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::RTMIN, 0))), SenderClass::Other);
            for _ in 0..1024 {
                fill().map_err(|_| "fill RT queue")?;
            }
            kassert_eq!(fill(), Err(errno::EAGAIN));
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base);
            kassert!(!process.signal.lock().pending.any(|i| i.timer_id() == Some(id)));
            process.signal.lock().pending.discard_if(|i| i.signal() == Signal::RTMIN);
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base + 10);
            let _ = me.wait_signal(SigSet::of(&[Signal::RTMIN]), Some(time::now_ns())).map_err(|_| "after full")?;
            kassert_eq!(overrun(&process, id), Ok(1));
            delete(&process, id).map_err(|_| "cleanup RT")?;
            // SIG_IGN does not suppress CONT's process-tree side effect, but
            // a stale captured expiry must not perform even that side effect.
            set_current_action(Signal::CONT, super::KernelSigaction { handler: 1, ..Default::default() });
            let send_stop =
                || send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::STOP, 0))), SenderClass::SiUser);
            send_stop().map_err(|_| "stop")?;
            kassert!(matches!(process.next_signal(pid), super::Delivery::Stop(Signal::STOP)));
            let id = create(&process, 1, 0, 18, None, 0).map_err(|_| "CONT timer")?.id;
            set(&process, TimerId::Posix(id), 10, base, true).map_err(|_| "arm CONT")?;
            let instance = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().instance.unwrap();
            delete(&process, id).map_err(|_| "delete CONT")?;
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base);
            let stale_kept_stop = process.stopped.load(Ordering::SeqCst);
            let id = create(&process, 1, 0, 18, None, 0).map_err(|_| "live CONT")?.id;
            set(&process, TimerId::Posix(id), 10, base, true).map_err(|_| "arm live CONT")?;
            let instance = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().instance.unwrap();
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base);
            let continued = !process.stopped.load(Ordering::SeqCst);
            let rearmed = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().setting.deadline();
            delete(&process, id).map_err(|_| "cleanup CONT")?;
            kassert!(stale_kept_stop);
            kassert!(continued);
            kassert_eq!(rearmed, Some(base + 10));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn timer_delivery_forwards_before_worker_and_accounts_handlers() {
        use super::{current_process, current_thread, in_test_process, set_current_action};
        use crate::{kassert, kassert_eq};
        use lumen_signal::{Blocked, SigSet};
        in_test_process(|pid| {
            let _held_worker = WORKER_GATE.lock();
            let process = current_process();
            let me = current_thread();
            set_current_action(
                Signal::RTMIN,
                super::KernelSigaction { handler: 0x400000, flags: general::SA_SIGINFO as u64, ..Default::default() },
            );
            let base = time::now_ns() + 1_000_000_000_000;
            for worker_first in [false, true] {
                me.set_blocked(Blocked::new(SigSet::of(&[Signal::RTMIN])));
                let id = create(&process, 1, 0, 32, None, 0).map_err(|_| "create")?.id;
                set(&process, TimerId::Posix(id), 10, base, true).map_err(|_| "arm")?;
                let instance = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().instance.unwrap();
                PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base);
                // The worker owns an expired entry but has not taken siglock.
                let captured = TIMERS.lock().remove(&(base + 10, pid, instance)).ok_or("capture")?;
                if worker_first {
                    PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base + 35);
                }
                process.signal.lock().timers.test_now = Some(base + 35);
                me.set_blocked(Blocked::EMPTY);
                let super::Delivery::Handler(delivery) = process.next_signal(me.tid()) else {
                    return Err("timer handler missing");
                };
                kassert_eq!(i32::from_le_bytes(delivery.info.bytes()[20..24].try_into().unwrap()), 3);
                kassert_eq!(overrun(&process, id), Ok(3));
                // Resuming old captured work cannot generate another signal or
                // count the three periods a second time.
                if !worker_first {
                    PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base + 35);
                }
                kassert!(!process.signal.lock().pending.any_timer(instance));
                kassert!(TIMERS.lock().contains_key(&(base + 40, pid, instance)));
                PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base + 40);
                process.signal.lock().timers.test_now = Some(base + 40);
                me.set_blocked(Blocked::new(SigSet::of(&[Signal::RTMIN])));
                let info =
                    me.wait_signal(SigSet::of(&[Signal::RTMIN]), Some(time::now_ns())).map_err(|_| "next expiry")?;
                kassert_eq!(i32::from_le_bytes(info.bytes()[20..24].try_into().unwrap()), 0);
                kassert_eq!(overrun(&process, id), Ok(0));
                delete(&process, id).map_err(|_| "delete")?;
                drop(captured);
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn user_si_timer_cannot_suppress_or_account_kernel_timer() {
        use super::{KTEST_USER_VA, current_process, current_thread, in_test_process};
        use crate::{kassert, kassert_eq};
        use lumen_signal::{Blocked, SigSet};
        in_test_process(|pid| {
            let _held_worker = WORKER_GATE.lock();
            let process = current_process();
            let me = current_thread();
            let wanted = SigSet::of(&[Signal::RTMIN]);
            me.set_blocked(Blocked::new(wanted));
            let id = create(&process, 1, 0, 32, Some(456), 0).map_err(|_| "create")?.id;
            let base = time::now_ns() + 1_000_000_000_000;
            set(&process, TimerId::Posix(id), 10, base, true).map_err(|_| "arm")?;
            let instance = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().instance.unwrap();
            let mut fake = SigInfo::timer(Signal::RTMIN, id, 123);
            fake.set_timer_overrun(77);
            crate::user::copy_to_user(KTEST_USER_VA, fake.bytes()).map_err(|_| "fake image")?;
            let send_fake = || {
                let mut frame = crate::arch::syscall::SyscallFrame {
                    rax: general::__NR_rt_sigqueueinfo as u64,
                    rdi: pid as u64,
                    rsi: 32,
                    rdx: KTEST_USER_VA,
                    ..Default::default()
                };
                crate::linux::syscall::dispatch(&mut frame);
                frame.rax
            };
            kassert_eq!(send_fake(), 0);
            kassert!(!process.signal.lock().pending.any_timer(instance));
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base);
            kassert!(process.signal.lock().pending.any_timer(instance));
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base + 20);
            process.signal.lock().timers.test_now = Some(base + 35);
            let got = me.wait_signal(wanted, Some(time::now_ns())).map_err(|_| "fake dequeue")?;
            kassert_eq!(got, fake);
            kassert_eq!(overrun(&process, id), Ok(0));
            kassert_eq!(process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().setting.overrun(), 2);
            let got = me.wait_signal(wanted, Some(time::now_ns())).map_err(|_| "real dequeue")?;
            kassert_eq!(u64::from_le_bytes(got.bytes()[24..32].try_into().unwrap()), 456);
            kassert_eq!(i32::from_le_bytes(got.bytes()[20..24].try_into().unwrap()), 3);
            kassert_eq!(overrun(&process, id), Ok(3));
            kassert_eq!(send_fake(), 0);
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base + 40);
            delete(&process, id).map_err(|_| "delete")?;
            kassert_eq!(me.wait_signal(wanted, Some(time::now_ns())).map_err(|_| "retained fake")?, fake);
            kassert!(!process.signal.lock().pending.any(|_| true));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn pending_timer_remainder_preserves_delivery_accounting() {
        use super::{current_process, current_thread, in_test_process};
        use crate::{kassert, kassert_eq};
        use lumen_signal::{Blocked, SigSet};
        in_test_process(|pid| {
            let _held_worker = WORKER_GATE.lock();
            let process = current_process();
            let me = current_thread();
            let wanted = SigSet::of(&[Signal::RTMIN]);
            me.set_blocked(Blocked::new(wanted));
            const MS: u64 = 1_000_000;
            let base = time::now_ns() + 1_000_000_000_000;
            for replace in [false, true] {
                let id = create(&process, 1, 0, 32, None, 0).map_err(|_| "create")?.id;
                let key = TimerId::Posix(id);
                set(&process, key, 10 * MS, base, true).map_err(|_| "arm")?;
                let instance = process.signal.lock().timers.get(key).unwrap().instance.unwrap();
                process.signal.lock().timers.test_now = Some(base + 35 * MS);
                // An overdue but not-yet-queued expiry still reports a positive
                // minimum; reading cannot consume the notification's first period.
                kassert_eq!(get(&process, key), Ok((10 * MS, 1)));
                PROCS.lock().expire_timer(&process, key, instance, base);
                if replace {
                    // Exercise old-value calculation without a preceding get.
                    kassert_eq!(set(&process, key, 0, 0, false), Ok((10 * MS, 5 * MS)));
                    kassert!(!process.signal.lock().pending.any_timer(instance));
                } else {
                    for _ in 0..2 {
                        kassert_eq!(get(&process, key), Ok((10 * MS, 5 * MS)));
                        kassert_eq!(overrun(&process, id), Ok(0));
                    }
                    kassert!(TIMERS.lock().contains_key(&(base + 10 * MS, pid, instance)));
                    let info = me.wait_signal(wanted, Some(time::now_ns())).map_err(|_| "consume")?;
                    kassert_eq!(i32::from_le_bytes(info.bytes()[20..24].try_into().unwrap()), 3);
                    kassert_eq!(overrun(&process, id), Ok(3));
                    PROCS.lock().expire_timer(&process, key, instance, base + 35 * MS);
                    kassert!(!process.signal.lock().pending.any_timer(instance));
                    kassert_eq!(get(&process, key), Ok((10 * MS, 5 * MS)));
                }
                delete(&process, id).map_err(|_| "delete")?;
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn refused_oneshot_expiry_reports_loss() {
        use super::{current_process, current_thread, in_test_process, send_process_checked};
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::SenderClass;
        use lumen_signal::{Blocked, SigSet};
        in_test_process(|pid| {
            let _held_worker = WORKER_GATE.lock();
            let process = current_process();
            let wanted = SigSet::of(&[Signal::RTMIN]);
            current_thread().set_blocked(Blocked::new(wanted));
            let id = create(&process, 1, 0, 32, None, 0).map_err(|_| "create")?.id;
            let key = TimerId::Posix(id);
            let base = time::now_ns() + 1_000_000_000_000;
            set(&process, key, 0, base, true).map_err(|_| "arm")?;
            let instance = process.signal.lock().timers.get(key).unwrap().instance.unwrap();
            let fill =
                || send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::RTMIN, 0))), SenderClass::Other);
            for _ in 0..1024 {
                fill().map_err(|_| "fill")?;
            }
            kassert_eq!(fill(), Err(errno::EAGAIN));
            PROCS.lock().expire_timer(&process, key, instance, base);
            kassert!(!process.signal.lock().pending.any_timer(instance));
            kassert_eq!(get(&process, key), Ok((0, 0)));
            kassert_eq!(overrun(&process, id), Ok(1));
            kassert_eq!(overrun(&process, id), Ok(1));
            process.signal.lock().pending.discard_if(|_| true);
            PROCS.lock().expire_timer(&process, key, instance, base + 100);
            kassert!(!process.signal.lock().pending.any(|_| true));
            kassert!(!TIMERS.lock().keys().any(|key| key.1 == pid));
            set(&process, key, 0, base + 100, true).map_err(|_| "rearm")?;
            kassert_eq!(overrun(&process, id), Ok(0));
            delete(&process, id).map_err(|_| "delete")?;
            Ok(())
        })?
    }
);

crate::ktest!(
    fn sleeping_timer_waiter_accounts_overruns_on_resume() {
        use super::{current_process, current_thread, in_test_process};
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::AtomicBool;
        use lumen_signal::{Blocked, SigSet};
        in_test_process(|_| {
            let _held_worker = WORKER_GATE.lock();
            let process = current_process();
            let me = current_thread();
            let wanted = SigSet::of(&[Signal::RTMIN]);
            me.set_blocked(Blocked::new(wanted));
            const MS: u64 = 1_000_000;
            let base = time::now_ns() + 1_000_000_000_000;
            let id = create(&process, 1, 0, 32, None, 0).map_err(|_| "create")?.id;
            set(&process, TimerId::Posix(id), 10 * MS, base, true).map_err(|_| "arm")?;
            let instance = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().instance.unwrap();
            let waiter = sched::current().ok_or("scheduler thread")?;
            let observed_blocked = AtomicBool::new(false);
            let info = sched::scope(|scope| -> Result<SigInfo, &'static str> {
                scope
                    .spawn_on(crate::arch::cpu_index(), "timer-wake", || {
                        while waiter.state() != sched::State::Blocked {
                            sched::yield_now();
                        }
                        observed_blocked.store(true, Ordering::Release);
                        process.signal.lock().timers.test_now = Some(base + 35 * MS);
                        PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base);
                    })
                    .map_err(|_| "spawn")?;
                me.wait_signal(wanted, None).map_err(|_| "sleeping wait")
            })
            .map_err(|_| "scope")??;
            kassert!(observed_blocked.load(Ordering::Acquire));
            kassert_eq!(i32::from_le_bytes(info.bytes()[20..24].try_into().unwrap()), 3);
            kassert_eq!(overrun(&process, id), Ok(3));
            kassert_eq!(me.blocked(), Blocked::new(wanted));
            delete(&process, id).map_err(|_| "delete")?;
            Ok(())
        })?
    }
);

crate::ktest!(
    fn exec_discards_private_timer_and_retains_user_siginfo() {
        use super::{current_process, current_thread, in_test_process, send_thread};
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::SenderClass;
        use lumen_signal::{Blocked, SigSet};
        in_test_process(|_| {
            let _held_worker = WORKER_GATE.lock();
            let process = current_process();
            let me = current_thread();
            let wanted = SigSet::of(&[Signal::RTMIN]);
            me.set_blocked(Blocked::new(wanted));
            let id = create(&process, 1, 4, 32, None, me.tid()).map_err(|_| "create")?.id;
            let base = time::now_ns() + 1_000_000_000_000;
            set(&process, TimerId::Posix(id), 10, base, true).map_err(|_| "arm")?;
            let instance = process.signal.lock().timers.get(TimerId::Posix(id)).unwrap().instance.unwrap();
            PROCS.lock().expire_timer(&process, TimerId::Posix(id), instance, base);
            let fake = SigInfo::timer(Signal::RTMIN, id, 123);
            send_thread(None, me.tid(), Some(fake), SenderClass::Other).map_err(|_| "user signal")?;
            process.exec_signals(me.tid());
            kassert_eq!(overrun(&process, id), Err(errno::EINVAL));
            kassert!(!process.signal.lock().pending.any_timer(instance));
            kassert_eq!(me.wait_signal(wanted, Some(time::now_ns())).map_err(|_| "retained user")?, fake);
            kassert!(!process.signal.lock().pending.any(|_| true));
            kassert!(!TIMERS.lock().keys().any(|key| key.1 == process.pid()));
            Ok(())
        })?
    }
);
