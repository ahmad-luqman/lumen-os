//! A randomized model test: random fork, exit, wait, setpgid, setsid, stop,
//! continue, exec and tcsetpgrp sequences run on a [`ProcTree`] and on a
//! naive reference model written here (flat maps and linear scans, sharing
//! no code with the crate), compared after every step, with the issue's
//! invariants checked throughout:
//! - every live non-init process has a live parent;
//! - a zombie is reaped exactly once;
//! - a pgid always names a live group leader or an existing group in the
//!   same session (all members of a group share one session).
//!
//! The model's exit transliterates Linux's `exit_notify` loop (each child
//! moved, then its group tested while the later children still name the
//! exiting process; a `SIGCONT` ending a group's stops at once), not the
//! crate's closed form, so it checks the crate's orphaned-group rule. Its
//! wait, setpgid and tcsetpgrp transliterate the crate's own rules: their
//! independent coverage is the unit tests.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use super::{INIT, Rng, sig};
use crate::wait::{
    __WALL, __WCLONE, __WNOTHREAD, P_ALL, P_PGID, P_PID, WCONTINUED, WEXITED, WNOHANG, WNOWAIT, WSTOPPED,
};
use crate::{
    Bug, Controlling, Errno, Error, ExitAction, ExitStatus, ProcState, ProcTree, Report, Signal, Sigttou, StateChanged,
    Tcsetpgrp, WaitReady, WaitTarget,
};

const MAX_PID: u32 = 16;
const SEEDS: u64 = 300;
const STEPS: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MState {
    Run,
    Stop(Signal, bool),
    Cont(bool),
    Zombie(ExitStatus),
}

#[derive(Clone, Debug)]
struct MProc {
    parent: Option<u32>,
    children: Vec<u32>,
    pgid: u32,
    sid: u32,
    state: MState,
    execed: bool,
    slot: u64,
}

struct Model {
    procs: BTreeMap<u32, MProc>,
    last: u32,
}

#[derive(Default)]
struct Seen {
    reaped: u32,
    peeked: u32,
    stopped_reports: u32,
    continued_reports: u32,
    again: u32,
    wrapped: u32,
    adopted_zombies: u32,
    orphaned: u32,
    other_reaper: u32,
    no_reaper: u32,
    caller_bugs: u32,
    setpgid_ok: u32,
    setpgid_err: BTreeSet<u32>,
    setsid_ok: u32,
    sigttou: u32,
    tc_set: u32,
    tc_err: BTreeSet<u32>,
}

fn user<T>(e: Errno) -> Result<T, Error> {
    Err(Error::User(e))
}

fn not_live<T>(pid: u32) -> Result<T, Error> {
    Err(Error::Bug(Bug::NotLive { pid }))
}

impl Model {
    fn new() -> Self {
        let mut procs = BTreeMap::new();
        procs.insert(
            INIT,
            MProc {
                parent: None,
                children: Vec::new(),
                pgid: INIT,
                sid: INIT,
                state: MState::Run,
                execed: false,
                slot: 0,
            },
        );
        Self { procs, last: 1 }
    }

    fn live(&self, pid: u32) -> bool {
        self.procs.get(&pid).is_some_and(|p| !matches!(p.state, MState::Zombie(_)))
    }

    fn busy(&self, n: u32) -> bool {
        self.procs.contains_key(&n) || self.procs.values().any(|p| p.pgid == n || p.sid == n)
    }

    /// The pid the crate's cyclic allocation hands out next: after the last
    /// one, wrapping from `MAX_PID` to 2.
    fn next_pid(&self) -> Option<u32> {
        let span = MAX_PID - 1; // 2..=MAX_PID
        let off = if self.last < 2 { span - 1 } else { self.last - 2 };
        (1..=span).map(|k| 2 + (off + k) % span).find(|&n| !self.busy(n))
    }

    fn group_exists(&self, g: u32) -> bool {
        self.procs.values().any(|p| p.pgid == g)
    }

    fn is_descendant_or_self(&self, mut pid: u32, ancestor: u32) -> bool {
        for _ in 0..=MAX_PID {
            if pid == ancestor {
                return true;
            }
            match self.procs[&pid].parent {
                Some(pp) => pid = pp,
                None => return false,
            }
        }
        panic!("a cycle above {pid}");
    }

    /// Linux's `will_become_orphaned_pgrp(g, ignored)`.
    fn will_become_orphaned(&self, g: u32, ignored: Option<u32>) -> bool {
        for (&pid, p) in &self.procs {
            if p.pgid != g || Some(pid) == ignored || matches!(p.state, MState::Zombie(_)) {
                continue;
            }
            // `is_global_init(p->real_parent)`; a root has no parent at all.
            let Some(pp) = p.parent.filter(|&pp| pp != INIT) else { continue };
            let q = &self.procs[&pp];
            if q.pgid != g && q.sid == p.sid {
                return false;
            }
        }
        true
    }

    fn orphaned(&self, g: u32) -> bool {
        self.will_become_orphaned(g, None)
    }

    /// Linux's `has_stopped_jobs`, `ignored` (an exiting process: its group
    /// exit ended its stop) left out.
    fn has_stopped(&self, g: u32, ignored: Option<u32>) -> bool {
        self.procs.iter().any(|(&pid, p)| p.pgid == g && Some(pid) != ignored && matches!(p.state, MState::Stop(..)))
    }

    fn fork(&mut self, parent: u32, slot: u64) -> Result<u32, Error> {
        if !self.live(parent) {
            return not_live(parent);
        }
        let pid = self.next_pid().ok_or(Error::User(Errno::Again))?;
        let (pgid, sid) = (self.procs[&parent].pgid, self.procs[&parent].sid);
        self.procs.insert(
            pid,
            MProc { parent: Some(parent), children: Vec::new(), pgid, sid, state: MState::Run, execed: false, slot },
        );
        self.procs.get_mut(&parent).unwrap().children.push(pid);
        self.last = pid;
        Ok(pid)
    }

    /// Linux's `exit_notify`, step by step: `forget_original_parent` (each
    /// child's parent set to the reaper, then `reparent_leader`: notify a
    /// zombie, `kill_orphaned_pgrp(child, father)`), the children spliced
    /// onto the reaper's list, `kill_orphaned_pgrp(tsk, NULL)`, the zombie,
    /// `do_notify_parent`. A group sent `SIGCONT` has no stopped member for
    /// the rest of the exit. `reaper` is `None` only for a childless exit.
    fn exit(&mut self, pid: u32, status: ExitStatus, reaper: Option<u32>) -> Vec<ExitAction> {
        let kids = core::mem::take(&mut self.procs.get_mut(&pid).unwrap().children);
        let reaper = match reaper {
            Some(r) => r,
            None => {
                assert!(kids.is_empty(), "no reaper for {pid}'s children");
                pid // never used: no child to move
            }
        };
        let (my_pgid, my_sid) = (self.procs[&pid].pgid, self.procs[&pid].sid);
        let mut continued = BTreeSet::new();
        let mut actions = Vec::new();
        for &c in &kids {
            self.procs.get_mut(&c).unwrap().parent = Some(reaper);
            let child = self.procs[&c].clone();
            if matches!(child.state, MState::Zombie(_)) {
                actions.push(ExitAction::Notify { parent: reaper, child: c });
            }
            let g = child.pgid;
            if my_pgid != g
                && my_sid == child.sid
                && self.will_become_orphaned(g, None)
                && !continued.contains(&g)
                && self.has_stopped(g, None)
            {
                actions.push(ExitAction::HangupThenContinue { pgid: g });
                continued.insert(g);
            }
        }
        self.procs.get_mut(&reaper).unwrap().children.extend(kids);
        let parent = self.procs[&pid].parent;
        if let Some(pp) = parent {
            let q = &self.procs[&pp];
            if q.pgid != my_pgid
                && q.sid == my_sid
                && self.will_become_orphaned(my_pgid, Some(pid))
                && !continued.contains(&my_pgid)
                && self.has_stopped(my_pgid, Some(pid))
            {
                actions.push(ExitAction::HangupThenContinue { pgid: my_pgid });
            }
        }
        self.procs.get_mut(&pid).unwrap().state = MState::Zombie(status);
        actions.push(match parent {
            Some(parent) => ExitAction::Notify { parent, child: pid },
            None => ExitAction::ReleaseNow { child: pid },
        });
        actions
    }

    /// Linux's `kernel_wait4`/`kernel_waitid_prepare` checks, after the
    /// options (only valid ones are generated): the target.
    fn target(&self, caller: u32, waitid: bool, idtype: i32, id: i32) -> Result<WaitTarget, Error> {
        if !self.live(caller) {
            return not_live(caller);
        }
        let own = self.procs[&caller].pgid;
        if waitid {
            match idtype {
                P_ALL => Ok(WaitTarget::Any),
                P_PID if id > 0 => Ok(WaitTarget::Pid(id as u32)),
                P_PGID if id == 0 => Ok(WaitTarget::Pgid(own)),
                P_PGID if id > 0 => Ok(WaitTarget::Pgid(id as u32)),
                _ => user(Errno::Inval),
            }
        } else {
            match id {
                i32::MIN => user(Errno::Srch),
                -1 => Ok(WaitTarget::Any),
                0 => Ok(WaitTarget::Pgid(own)),
                n if n > 0 => Ok(WaitTarget::Pid(n as u32)),
                n => Ok(WaitTarget::Pgid((-n) as u32)),
            }
        }
    }

    fn wait(&mut self, caller: u32, target: WaitTarget, waitid: bool, bits: u32) -> Result<WaitReady, Error> {
        if !self.live(caller) {
            return not_live(caller);
        }
        if bits & __WCLONE != 0 && bits & __WALL == 0 {
            return user(Errno::Child);
        }
        let exited = !waitid || bits & WEXITED != 0;
        let keep = bits & WNOWAIT != 0;
        let mut live_match = false;
        for c in self.procs[&caller].children.clone() {
            let p = self.procs[&c].clone();
            let hit = match target {
                WaitTarget::Any => true,
                WaitTarget::Pid(n) => c == n,
                WaitTarget::Pgid(g) => p.pgid == g,
            };
            if !hit {
                continue;
            }
            match p.state {
                MState::Zombie(status) if exited => {
                    if keep {
                        return Ok(WaitReady::Reported(Report::Zombie { pid: c, status, rusage_slot: p.slot }));
                    }
                    self.procs.remove(&c);
                    self.procs.get_mut(&caller).unwrap().children.retain(|&x| x != c);
                    return Ok(WaitReady::Reported(Report::Reaped { pid: c, status, rusage_slot: p.slot }));
                }
                MState::Zombie(_) => {}
                MState::Stop(signal, false) if bits & WSTOPPED != 0 => {
                    if !keep {
                        self.procs.get_mut(&c).unwrap().state = MState::Stop(signal, true);
                    }
                    return Ok(WaitReady::Reported(Report::Stopped { pid: c, signal }));
                }
                MState::Cont(false) if bits & WCONTINUED != 0 => {
                    if !keep {
                        self.procs.get_mut(&c).unwrap().state = MState::Cont(true);
                    }
                    return Ok(WaitReady::Reported(Report::Continued { pid: c }));
                }
                _ => live_match = true,
            }
        }
        if live_match { Ok(WaitReady::NoneReady) } else { user(Errno::Child) }
    }

    fn setpgid(&mut self, caller: u32, pid: i32, pgid: i32) -> Result<(), Error> {
        if !self.live(caller) {
            return not_live(caller);
        }
        let pid = if pid == 0 { caller as i32 } else { pid };
        let pgid = if pgid == 0 { pid } else { pgid };
        if pgid < 0 {
            return user(Errno::Inval);
        }
        if pid < 0 || !self.procs.contains_key(&(pid as u32)) {
            return user(Errno::Srch);
        }
        let pid = pid as u32;
        let target = self.procs[&pid].clone();
        let me = self.procs[&caller].clone();
        if target.parent == Some(caller) {
            if target.sid != me.sid {
                return user(Errno::Perm);
            }
            if target.execed {
                return user(Errno::Acces);
            }
        } else if pid != caller {
            return user(Errno::Srch);
        }
        if target.sid == pid {
            return user(Errno::Perm);
        }
        let pgid = pgid as u32;
        if pgid != pid && !self.procs.values().any(|p| p.pgid == pgid && p.sid == me.sid) {
            return user(Errno::Perm);
        }
        self.procs.get_mut(&pid).unwrap().pgid = pgid;
        Ok(())
    }

    fn setsid(&mut self, caller: u32) -> Result<u32, Error> {
        if !self.live(caller) {
            return not_live(caller);
        }
        if self.group_exists(caller) {
            return user(Errno::Perm);
        }
        let p = self.procs.get_mut(&caller).unwrap();
        p.pgid = caller;
        p.sid = caller;
        Ok(caller)
    }

    fn tcsetpgrp(&self, caller: u32, tty: Option<Controlling>, pgrp: i32, ttou: Sigttou) -> Result<Tcsetpgrp, Error> {
        if !self.live(caller) {
            return not_live(caller);
        }
        let me = &self.procs[&caller];
        if tty.and_then(|t| t.foreground).is_some_and(|fg| fg != me.pgid) && ttou == Sigttou::Delivered {
            return if self.orphaned(me.pgid) { user(Errno::NoTty) } else { Ok(Tcsetpgrp::Sigttou { pgrp: me.pgid }) };
        }
        if pgrp < 0 {
            return user(Errno::Inval);
        }
        if tty.and_then(|t| t.session) != Some(me.sid) {
            return user(Errno::NoTty);
        }
        let n = pgrp as u32;
        if !self.busy(n) {
            return user(Errno::Srch);
        }
        let session =
            self.procs.values().find(|p| p.pgid == n).map(|p| p.sid).or_else(|| self.procs.get(&n).map(|p| p.sid));
        if session != Some(me.sid) {
            return user(Errno::Perm);
        }
        Ok(Tcsetpgrp::Set(n))
    }

    fn stop(&mut self, pid: u32, signal: Signal) -> Result<Option<StateChanged>, Errno> {
        if !self.live(pid) {
            return Err(Errno::Srch);
        }
        let p = self.procs.get_mut(&pid).unwrap();
        if matches!(p.state, MState::Stop(..)) {
            return Ok(None);
        }
        p.state = MState::Stop(signal, false);
        Ok(Some(StateChanged { parent: p.parent }))
    }

    fn cont(&mut self, pid: u32) -> Result<Option<StateChanged>, Errno> {
        if !self.live(pid) {
            return Err(Errno::Srch);
        }
        let p = self.procs.get_mut(&pid).unwrap();
        if !matches!(p.state, MState::Stop(..)) {
            return Ok(None);
        }
        p.state = MState::Cont(false);
        Ok(Some(StateChanged { parent: p.parent }))
    }
}

/// The tree and the model agree on every process, field by field.
fn assert_same(t: &ProcTree, m: &Model, step: &str) {
    let pids: Vec<u32> = t.pids().collect();
    let mpids: Vec<u32> = m.procs.keys().copied().collect();
    assert_eq!(pids, mpids, "{step}: pids");
    for (&pid, p) in &m.procs {
        assert_eq!(t.parent(pid), Some(p.parent), "{step}: parent of {pid}");
        assert_eq!(t.children(pid), p.children.as_slice(), "{step}: children of {pid}");
        assert_eq!(t.pgid(pid), Some(p.pgid), "{step}: pgid of {pid}");
        assert_eq!(t.sid(pid), Some(p.sid), "{step}: sid of {pid}");
        assert_eq!(t.has_execed(pid), Some(p.execed), "{step}: exec'd {pid}");
        assert_eq!(t.rusage_slot(pid), Some(p.slot), "{step}: slot of {pid}");
        let state = match p.state {
            MState::Run => ProcState::Running,
            MState::Stop(signal, reported) => ProcState::Stopped { signal, reported },
            MState::Cont(reported) => ProcState::Continued { reported },
            MState::Zombie(s) => ProcState::Zombie(s),
        };
        assert_eq!(t.state(pid), Some(state), "{step}: state of {pid}");
    }
    for g in m.procs.values().map(|p| p.pgid).collect::<BTreeSet<_>>() {
        assert_eq!(t.is_orphaned_pgrp(g), m.orphaned(g), "{step}: orphaned {g}");
    }
}

/// The issue's invariants, on the tree.
fn assert_invariants(t: &ProcTree, step: &str) {
    t.check_invariants();
    let live = |pid: u32| t.state(pid).is_some_and(|s| !matches!(s, ProcState::Zombie(_)));
    for pid in t.pids() {
        if pid != INIT {
            let parent = t.parent(pid).unwrap();
            assert!(parent.is_some_and(live), "{step}: {pid}'s parent {parent:?} isn't live");
        }
        let (pgid, sid) = (t.pgid(pid).unwrap(), t.sid(pid).unwrap());
        assert!(t.pgrp_members(pgid).all(|m| t.sid(m) == Some(sid)), "{step}: group {pgid} spans sessions");
    }
}

fn random_status(rng: &mut Rng) -> ExitStatus {
    if rng.chance(70) {
        ExitStatus::exited(rng.below(512) as i32 - 128)
    } else {
        ExitStatus::Killed { signal: sig(1 + rng.below(64) as u8), core: rng.chance(30) }
    }
}

fn random_number(rng: &mut Rng, m: &Model) -> i32 {
    match rng.below(4) {
        0 => rng.below(u64::from(MAX_PID) + 3) as i32,
        1 => -(rng.below(4) as i32),
        _ => {
            let pids: Vec<u32> = m.procs.keys().copied().collect();
            rng.pick(&pids).unwrap() as i32
        }
    }
}

/// Valid options only: the request tests pin `EINVAL`.
fn random_options(rng: &mut Rng, waitid: bool) -> u32 {
    let flags: &[u32] = if waitid {
        &[WNOHANG, WNOWAIT, WEXITED, WEXITED, WSTOPPED, WCONTINUED, __WNOTHREAD, __WALL, __WCLONE]
    } else {
        &[WNOHANG, WSTOPPED, WSTOPPED, WCONTINUED, __WNOTHREAD, __WALL, __WCLONE]
    };
    let mut bits = 0;
    for &f in flags {
        if rng.chance(30) {
            bits |= f;
        }
    }
    if waitid && bits & (WEXITED | WSTOPPED | WCONTINUED) == 0 {
        bits |= [WEXITED, WSTOPPED, WCONTINUED][rng.below(3) as usize];
    }
    bits
}

fn run(seed: u64, seen: &mut Seen) {
    let mut rng = Rng::new(seed);
    let mut t = ProcTree::new(MAX_PID);
    t.spawn_init(0).unwrap();
    let mut m = Model::new();
    let mut zombies = BTreeSet::new();
    let (mut exits, mut reaps) = (0u32, 0u32);
    let mut last_pid = 1;

    for step in 0..STEPS {
        let live: Vec<u32> = m.procs.keys().copied().filter(|&p| m.live(p)).collect();
        let others: Vec<u32> = live.iter().copied().filter(|&p| p != INIT).collect();
        let caller = rng.pick(&live).unwrap();
        let label = alloc::format!("seed {seed} step {step}");
        match rng.below(100) {
            0..=24 => {
                let slot = rng.next();
                let got = t.fork(caller, slot);
                assert_eq!(got, m.fork(caller, slot), "{label}: fork by {caller}");
                match got {
                    Ok(pid) => {
                        if pid < last_pid {
                            seen.wrapped += 1;
                        }
                        last_pid = pid;
                    }
                    Err(_) => seen.again += 1,
                }
            }
            25..=39 => {
                let Some(pid) = rng.pick(&others) else { continue };
                // Mostly init; sometimes another live process outside pid's subtree.
                let reapers: Vec<u32> = live.iter().copied().filter(|&r| !m.is_descendant_or_self(r, pid)).collect();
                let childless = m.procs[&pid].children.is_empty();
                let reaper = match rng.below(100) {
                    0..=19 if childless => None,
                    0..=39 => Some(rng.pick(&reapers).unwrap_or(INIT)),
                    _ => Some(INIT),
                };
                let status = random_status(&mut rng);
                let got: Vec<ExitAction> = t.exit(pid, status, reaper).expect("exit").actions().collect();
                let want = m.exit(pid, status, reaper);
                assert_eq!(got, want, "{label}: exit {pid} to {reaper:?}");
                for a in &want {
                    match a {
                        ExitAction::HangupThenContinue { .. } => seen.orphaned += 1,
                        ExitAction::Notify { child, .. } if *child != pid => seen.adopted_zombies += 1,
                        _ => {}
                    }
                }
                match reaper {
                    Some(r) if r != INIT && !m.procs[&r].children.is_empty() => seen.other_reaper += 1,
                    None => seen.no_reaper += 1,
                    _ => {}
                }
                assert!(zombies.insert(pid), "{label}: {pid} exited twice");
                exits += 1;
            }
            40..=64 => {
                // Sometimes a caller that isn't live.
                let caller = if rng.chance(5) { rng.below(u64::from(MAX_PID) + 2) as u32 } else { caller };
                let waitid = rng.chance(40);
                let bits = random_options(&mut rng, waitid);
                let (idtype, id) = if waitid {
                    ([P_ALL, P_PID, P_PGID][rng.below(3) as usize], random_number(&mut rng, &m))
                } else {
                    let pid = match rng.below(3) {
                        0 => -1,
                        1 => random_number(&mut rng, &m),
                        _ => -random_number(&mut rng, &m),
                    };
                    (0, pid)
                };
                let request =
                    if waitid { t.waitid_request(caller, idtype, id, bits) } else { t.wait4_request(caller, id, bits) };
                let want_target = m.target(caller, waitid, idtype, id);
                assert_eq!(request.map(|r| r.target()), want_target, "{label}: request by {caller}: {idtype} {id}");
                let (Ok(request), Ok(target)) = (request, want_target) else {
                    if !m.live(caller) {
                        seen.caller_bugs += 1;
                    }
                    continue;
                };
                let got = t.wait(request);
                let want = m.wait(caller, target, waitid, bits);
                assert_eq!(got, want, "{label}: wait by {caller} for {target:?} with {bits:#x}");
                match got {
                    Ok(WaitReady::Reported(Report::Reaped { pid, .. })) => {
                        assert!(zombies.remove(&pid), "{label}: reaped {pid}, not a zombie");
                        reaps += 1;
                        seen.reaped += 1;
                    }
                    Ok(WaitReady::Reported(Report::Zombie { pid, .. })) => {
                        assert!(zombies.contains(&pid));
                        seen.peeked += 1;
                    }
                    Ok(WaitReady::Reported(Report::Stopped { .. })) => seen.stopped_reports += 1,
                    Ok(WaitReady::Reported(Report::Continued { .. })) => seen.continued_reports += 1,
                    _ => {}
                }
            }
            65..=76 => {
                let kids: Vec<u32> = m.procs[&caller].children.clone();
                let pid = match rng.below(4) {
                    0 => 0,
                    1 => rng.pick(&kids).map_or(caller as i32, |k| k as i32),
                    _ => random_number(&mut rng, &m),
                };
                let pgid = match rng.below(4) {
                    0 => 0,
                    1 => {
                        let groups: Vec<u32> = m.procs.values().map(|p| p.pgid).collect();
                        rng.pick(&groups).unwrap() as i32
                    }
                    _ => random_number(&mut rng, &m),
                };
                let got = t.setpgid(caller, pid, pgid);
                assert_eq!(got, m.setpgid(caller, pid, pgid), "{label}: setpgid({pid}, {pgid}) by {caller}");
                match got {
                    Ok(()) => seen.setpgid_ok += 1,
                    Err(Error::User(e)) => {
                        seen.setpgid_err.insert(e.to_errno());
                    }
                    Err(e) => panic!("{label}: setpgid {e:?}"),
                }
            }
            77..=81 => {
                let got = t.setsid(caller);
                assert_eq!(got, m.setsid(caller), "{label}: setsid by {caller}");
                if got.is_ok() {
                    seen.setsid_ok += 1;
                }
            }
            82..=88 => {
                let Some(pid) = rng.pick(&others) else { continue };
                let signal =
                    [Signal::SIGSTOP, Signal::SIGTSTP, Signal::SIGTTIN, Signal::SIGTTOU][rng.below(4) as usize];
                assert_eq!(t.stop(pid, signal), m.stop(pid, signal), "{label}: stop {pid}");
            }
            89..=93 => {
                let Some(pid) = rng.pick(&others) else { continue };
                assert_eq!(t.cont(pid), m.cont(pid), "{label}: cont {pid}");
            }
            94..=95 => {
                let Some(pid) = rng.pick(&others) else { continue };
                t.exec(pid).unwrap();
                m.procs.get_mut(&pid).unwrap().execed = true;
            }
            _ => {
                let me = &m.procs[&caller];
                let pick_id = |rng: &mut Rng, own: u32| match rng.below(3) {
                    0 => None,
                    1 => Some(own),
                    _ => Some(random_number(rng, &m).unsigned_abs()),
                };
                let tty = if rng.chance(80) {
                    Some(Controlling { session: pick_id(&mut rng, me.sid), foreground: pick_id(&mut rng, me.pgid) })
                } else {
                    None
                };
                let pgrp = random_number(&mut rng, &m);
                let ttou = if rng.chance(70) { Sigttou::Delivered } else { Sigttou::BlockedOrIgnored };
                let got = t.tcsetpgrp(caller, tty, pgrp, ttou);
                assert_eq!(got, m.tcsetpgrp(caller, tty, pgrp, ttou), "{label}: tcsetpgrp {tty:?} {pgrp}");
                match got {
                    Ok(Tcsetpgrp::Set(_)) => seen.tc_set += 1,
                    Ok(Tcsetpgrp::Sigttou { .. }) => seen.sigttou += 1,
                    Err(Error::User(e)) => {
                        seen.tc_err.insert(e.to_errno());
                    }
                    Err(e) => panic!("{label}: tcsetpgrp {e:?}"),
                }
                let who = random_number(&mut rng, &m);
                let want =
                    if who == 0 { Some(caller) } else { u32::try_from(who).ok().filter(|p| m.procs.contains_key(p)) };
                let want_pgid = want.map(|p| m.procs[&p].pgid).ok_or(Error::User(Errno::Srch));
                let want_sid = want.map(|p| m.procs[&p].sid).ok_or(Error::User(Errno::Srch));
                assert_eq!(t.getpgid(caller, who), want_pgid, "{label}: getpgid {who}");
                assert_eq!(t.getsid(caller, who), want_sid, "{label}: getsid {who}");
            }
        }
        assert_same(&t, &m, &label);
        assert_invariants(&t, &label);
    }

    // Everyone else exits; init reaps every zombie, each once. Each loop
    // ends within MAX_PID rounds: one process exits, or is reaped, a round.
    for _ in 0..=MAX_PID {
        let others: Vec<u32> = m.procs.keys().copied().filter(|&p| p != INIT && m.live(p)).collect();
        let Some(pid) = rng.pick(&others) else { break };
        let status = random_status(&mut rng);
        let got: Vec<ExitAction> = t.exit(pid, status, Some(INIT)).unwrap().actions().collect();
        assert_eq!(got, m.exit(pid, status, Some(INIT)), "seed {seed}: final exit {pid}");
        assert!(zombies.insert(pid));
        exits += 1;
    }
    for _ in 0..=MAX_PID {
        let request = t.wait4_request(INIT, -1, WNOHANG).unwrap();
        let got = t.wait(request);
        assert_eq!(got, m.wait(INIT, WaitTarget::Any, false, WNOHANG), "seed {seed}: drain");
        match got {
            Ok(WaitReady::Reported(Report::Reaped { pid, .. })) => {
                assert!(zombies.remove(&pid), "seed {seed}: reaped {pid} twice");
                reaps += 1;
            }
            Err(Error::User(Errno::Child)) => break,
            other => panic!("seed {seed}: drain got {other:?}"),
        }
    }
    assert!(zombies.is_empty(), "seed {seed}: unreaped {zombies:?}");
    assert_eq!(exits, reaps, "seed {seed}");
    assert_eq!(t.pids().collect::<Vec<_>>(), [INIT]);
    assert_invariants(&t, "end");
}

#[test]
fn random_fork_exit_wait_setpgid_sequences_keep_the_invariants() {
    let mut seen = Seen::default();
    for seed in 0..SEEDS {
        run(seed, &mut seen);
    }
    // The runs reached every interesting case.
    assert!(seen.reaped > 0 && seen.peeked > 0, "reaps {} peeks {}", seen.reaped, seen.peeked);
    assert!(seen.stopped_reports > 0 && seen.continued_reports > 0);
    assert!(seen.again > 0, "no fork ran out of pids");
    assert!(seen.wrapped > 0, "no pid wrapped");
    assert!(seen.adopted_zombies > 0, "no zombie adopted");
    assert!(seen.orphaned > 0, "no group orphaned with a stopped member");
    assert!(seen.other_reaper > 0, "no children adopted by a reaper other than init");
    assert!(seen.no_reaper > 0, "no childless exit without a reaper");
    assert!(seen.caller_bugs > 0, "no wait by a caller that isn't live");
    assert!(seen.setpgid_ok > 0 && seen.setsid_ok > 0);
    for e in [Errno::Perm, Errno::Srch, Errno::Acces, Errno::Inval] {
        assert!(seen.setpgid_err.contains(&e.to_errno()), "setpgid never gave {e}");
    }
    assert!(seen.tc_set > 0 && seen.sigttou > 0);
    for e in [Errno::Perm, Errno::Srch, Errno::Inval, Errno::NoTty] {
        assert!(seen.tc_err.contains(&e.to_errno()), "tcsetpgrp never gave {e}");
    }
}
