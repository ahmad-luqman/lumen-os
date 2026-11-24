use linux_raw_sys::{errno, general as g};
use lumen_signal::frame::Registers;
use lumen_signal::pending::*;
use lumen_signal::restart::*;
use lumen_signal::{DefaultAction, SigSet, Signal};

fn sig(number: u32) -> Signal {
    Signal::new(number).unwrap()
}
fn info(number: u32, code: i32) -> SigInfo {
    SigInfo::new(sig(number), code)
}

#[test]
fn signal_sets_and_unblockables() {
    for invalid in [0, 65, 256, u32::MAX] {
        assert_eq!(Signal::new(invalid), None);
    }
    let mut set = SigSet::EMPTY;
    for number in 1..=64 {
        let signal = sig(number);
        assert_eq!(signal.number(), number);
        assert_eq!(signal.is_realtime(), number >= 32);
        assert_eq!(signal.is_unblockable(), number == 9 || number == 19);
        assert!(!set.contains(signal));
        set.insert(signal);
        assert!(set.contains(signal));
        assert_eq!(set.0, u64::MAX >> (64 - number));
    }
    assert_eq!(set, SigSet::ALL);
    let stripped = set.unblockable_stripped();
    assert_eq!(stripped.0, !(1 << 8) & !(1 << 18));
    for number in 1..=64 {
        assert_eq!(stripped.contains(sig(number)), number != 9 && number != 19);
        set.remove(sig(number));
        assert!(!set.contains(sig(number)));
    }
    assert_eq!(set, SigSet::EMPTY);
    for initial in [0, u64::MAX, 0x5555_5555_5555_5555, 0x8000_0000_0000_0001] {
        for operand in [0, u64::MAX, 0xaaaa_aaaa_aaaa_aaaa] {
            let keep = !(1 << 8) & !(1 << 18);
            for (op, expected) in
                [(g::SIG_BLOCK, initial | operand), (g::SIG_UNBLOCK, initial & !operand), (g::SIG_SETMASK, operand)]
            {
                assert_eq!(SigSet(initial).apply(op, SigSet(operand)), Ok(SigSet(expected & keep)));
            }
        }
    }
    assert_eq!(set.apply(3, SigSet::ALL), Err(-(errno::EINVAL as i32)));
}

#[test]
fn default_actions_match_signal7() {
    use DefaultAction::*;
    // signal(7)'s x86-64 table, in numeric order (aliases occupy one slot).
    let expected = [
        Term, Term, Core, Core, Core, Core, Core, Core, Term, Term, Core, Term, Term, Term, Term, Term, Ign, Cont,
        Stop, Stop, Stop, Stop, Ign, Core, Core, Term, Term, Ign, Term, Term, Core,
    ];
    for (index, action) in expected.into_iter().enumerate() {
        assert_eq!(sig(index as u32 + 1).default_action(), action, "signal {}", index + 1);
    }
    for number in 32..=64 {
        assert_eq!(sig(number).default_action(), Term);
    }
}

#[test]
fn standard_coalescing_and_bounded_realtime_fifo() {
    let mut pending = Pending::new(3);
    assert_eq!(pending.enqueue(info(10, 101)), Ok(Enqueued::Queued));
    assert_eq!(pending.enqueue(info(10, 202)), Ok(Enqueued::Coalesced));
    for item in [info(64, 1), info(32, 2), info(64, 3)] {
        pending.enqueue(item).unwrap();
    }
    assert_eq!(pending.enqueue(info(33, 4)), Err(PendingError::Full));
    assert_eq!(PendingError::Full.errno(), -11);
    assert!(!pending.signals().contains(sig(33)));
    let mut mask = SigSet::EMPTY;
    mask.insert(sig(10));
    assert_eq!(pending.dequeue(mask), Some(info(32, 2)));
    assert_eq!(pending.enqueue(info(32, 5)), Ok(Enqueued::Queued));
    for expected in [info(10, 101), info(32, 5), info(64, 1), info(64, 3)] {
        assert_eq!(pending.dequeue(SigSet::EMPTY), Some(expected));
    }
    assert_eq!(pending.signals(), SigSet::EMPTY);
    assert_eq!(pending.dequeue(SigSet::EMPTY), None);
    let mut zero = Pending::new(0);
    assert_eq!(zero.enqueue(info(32, 0)), Err(PendingError::Full));
    assert_eq!(zero.enqueue(info(1, 0)), Ok(Enqueued::Queued));
}

#[test]
fn dequeue_prioritizes_synchronous_signals() {
    let mut pending = Pending::new(33);
    for number in (1..=64).rev() {
        pending.enqueue(info(number, number as i32)).unwrap();
    }
    let synchronous = [4, 5, 7, 8, 11, 31];
    let order = synchronous.into_iter().chain((1..=64).filter(|n| !synchronous.contains(n)));
    for number in order {
        assert_eq!(pending.dequeue(SigSet::EMPTY), Some(info(number, number as i32)));
    }
    assert_eq!(pending.dequeue(SigSet::EMPTY), None);
    for number in [1, 4, 9, 19] {
        pending.enqueue(info(number, 0)).unwrap();
    }
    assert_eq!(pending.dequeue(SigSet::ALL), Some(info(9, 0)));
    assert_eq!(pending.dequeue(SigSet::ALL), Some(info(19, 0)));
    assert_eq!(pending.dequeue(SigSet::ALL), None);
    assert_eq!(pending.dequeue(SigSet(1 << 3)), Some(info(1, 0)));
    assert_eq!(pending.dequeue(SigSet::EMPTY), Some(info(4, 0)));
}

#[test]
fn private_pending_precedes_shared_and_threads_are_isolated() {
    let mut pending = PendingSignals::new(2, 2);
    pending.enqueue(Target::Thread(0), info(10, 1)).unwrap();
    pending.enqueue(Target::Thread(1), info(12, 2)).unwrap();
    pending.enqueue(Target::Process, info(4, 3)).unwrap();
    assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(Some(info(10, 1))));
    assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(Some(info(4, 3))));
    assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(None));
    assert_eq!(pending.dequeue(1, SigSet::EMPTY), Ok(Some(info(12, 2))));
    assert_eq!(pending.dequeue(2, SigSet::EMPTY), Err(PendingError::InvalidThread));
    assert_eq!(pending.enqueue(Target::Thread(2), info(9, 0)), Err(PendingError::InvalidThread));
    // Coalescing is per queue, even when private and shared carry the same number.
    pending.enqueue(Target::Process, info(10, 4)).unwrap();
    pending.enqueue(Target::Thread(0), info(10, 5)).unwrap();
    assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(Some(info(10, 5))));
    assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(Some(info(10, 4))));
}

#[test]
fn stop_continue_cancels_across_all_queues() {
    let targets = [Target::Process, Target::Thread(0), Target::Thread(1)];
    for stop in [19, 20, 21, 22] {
        for target in targets {
            let mut pending = PendingSignals::new(2, 1);
            pending.enqueue(Target::Process, info(12, 99)).unwrap();
            for destination in targets {
                pending.enqueue(destination, info(18, 0)).unwrap();
            }
            pending.enqueue(target, info(stop, 0)).unwrap();
            let mut seen = Vec::new();
            for thread in 0..2 {
                while let Some(item) = pending.dequeue(thread, SigSet::EMPTY).unwrap() {
                    seen.push(item.signal().number());
                }
            }
            seen.sort();
            assert_eq!(seen, [12, stop]);
        }
    }
    for target in targets {
        let mut pending = PendingSignals::new(2, 1);
        pending.enqueue(Target::Process, info(10, 99)).unwrap();
        for destination in targets {
            for stop in [19, 20, 21, 22] {
                pending.enqueue(destination, info(stop, 0)).unwrap();
            }
        }
        // Ignored SIGCONT must still cancel stops.
        pending.set_disposition(sig(18), Disposition::Ignore).unwrap();
        assert_eq!(pending.enqueue(target, info(18, 0)), Ok(Enqueued::Queued));
        let mut seen = Vec::new();
        for thread in 0..2 {
            while let Some(item) = pending.dequeue(thread, SigSet::EMPTY).unwrap() {
                seen.push(item.signal().number());
            }
        }
        seen.sort();
        assert_eq!(seen, [10, 18]);
    }
    let mut pending = PendingSignals::new(1, 0);
    pending.enqueue(Target::Process, info(18, 0)).unwrap();
    pending.set_disposition(sig(20), Disposition::Ignore).unwrap();
    assert_eq!(pending.enqueue(Target::Thread(0), info(20, 0)), Ok(Enqueued::Queued));
    assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(Some(info(20, 0))));
    assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(None));
}

#[test]
fn ignore_discards_pending_and_default_ignore_is_respected() {
    for (number, disposition) in [
        (10, Disposition::Ignore),
        (32, Disposition::Ignore),
        (17, Disposition::Default),
        (23, Disposition::Default),
        (28, Disposition::Default),
    ] {
        let mut pending = PendingSignals::new(2, 4);
        pending.set_disposition(sig(number), Disposition::Handler).unwrap();
        for destination in [Target::Process, Target::Thread(0), Target::Thread(1)] {
            pending.enqueue(destination, info(1, 77)).unwrap();
            pending.enqueue(destination, info(number, 1)).unwrap();
            pending.enqueue(destination, info(number, 2)).unwrap();
            pending.enqueue(destination, info(64, 88)).unwrap();
        }
        pending.set_disposition(sig(number), disposition).unwrap();
        let mut kept = Vec::new();
        for thread in 0..2 {
            while let Some(item) = pending.dequeue(thread, SigSet::EMPTY).unwrap() {
                kept.push(item);
            }
        }
        assert_eq!(kept, [info(1, 77), info(64, 88), info(1, 77), info(64, 88), info(1, 77), info(64, 88)]);
        assert_eq!(pending.disposition(sig(number)), disposition);
        // Queueing retains ignored signals for a caller implementing sigwait.
        assert_eq!(pending.enqueue(Target::Process, info(number, 3)), Ok(Enqueued::Queued));
        assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(Some(info(number, 3))));
        pending.set_disposition(sig(number), Disposition::Handler).unwrap();
        pending.enqueue(Target::Process, info(number, 4)).unwrap();
        assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(Some(info(number, 4))));
    }
    let mut pending = PendingSignals::new(1, 1);
    assert_eq!(pending.disposition(sig(17)), Disposition::Default);
    assert_eq!(pending.enqueue(Target::Process, info(17, 0)), Ok(Enqueued::Queued));
    assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(Some(info(17, 0))));
    pending.enqueue(Target::Process, info(10, 0)).unwrap();
    pending.set_disposition(sig(10), Disposition::Default).unwrap();
    assert_eq!(pending.dequeue(0, SigSet::EMPTY), Ok(Some(info(10, 0))));
    for number in [9, 19] {
        for disposition in [Disposition::Default, Disposition::Ignore, Disposition::Handler] {
            assert_eq!(pending.set_disposition(sig(number), disposition), Err(PendingError::Unchangeable));
        }
    }
}

#[test]
fn target_selection_checks_masks_states_and_rotation() {
    use ThreadState::*;
    for state in [Running, Sleeping, Stopped, Traced, Exiting] {
        for has_pending in [false, true] {
            for mask in [SigSet::EMPTY, SigSet::ALL] {
                let threads = [ThreadCandidate { state, mask, has_pending }];
                let ordinary = mask == SigSet::EMPTY && (state == Running || (state == Sleeping && !has_pending));
                assert_eq!(select_target(sig(10), &threads, 0), ordinary.then_some(0));
                assert_eq!(select_target(sig(9), &threads, 0), (state != Exiting).then_some(0));
            }
        }
    }
    let running = ThreadCandidate { state: Running, mask: SigSet::EMPTY, has_pending: true };
    assert_eq!(select_target(sig(19), &[ThreadCandidate { mask: SigSet::ALL, ..running }], 0), Some(0));
    let mut threads = [running; 3];
    assert_eq!(select_target(sig(10), &threads, 2), Some(2));
    threads[2].mask = SigSet::ALL;
    assert_eq!(select_target(sig(10), &threads, 2), Some(0));
    threads[0].state = Exiting;
    assert_eq!(select_target(sig(10), &threads, usize::MAX), Some(1));
    assert_eq!(select_target(sig(10), &[], 0), None);
}

#[test]
fn every_restart_table_row_and_register_effect() {
    use RestartCode::*;
    use RestartDecision::*;
    let original = Restart(RestartSyscall::Original);
    let block = Restart(RestartSyscall::RestartBlock);
    let rows = [
        (Sys, [original, ReturnEintr, original]),
        (NoIntr, [original; 3]),
        (NoHand, [original, ReturnEintr, ReturnEintr]),
        (RestartBlock, [block, ReturnEintr, ReturnEintr]),
    ];
    for (code, expected) in rows {
        assert_eq!(RestartCode::from_kernel_result(-(code as i64)), Some(code));
        for (handler, decision) in [None, Some(false), Some(true)].into_iter().zip(expected) {
            assert_eq!(decide(code, handler), decision);
            let before = Registers { rip: 0x400102, rax: (-(code as i64)) as u64, rbx: 777, ..Registers::default() };
            let mut actual = before;
            decision.apply(&mut actual, 44).unwrap();
            let mut wanted = before;
            match decision {
                ReturnEintr => wanted.rax = (-4i64) as u64,
                Restart(kind) => {
                    wanted.rip = 0x400100;
                    wanted.rax = if kind == RestartSyscall::Original { 44 } else { 219 };
                }
            }
            assert_eq!(actual, wanted);
        }
    }
    for result in [0, 512, -1, -4, -515, i64::MIN] {
        assert_eq!(RestartCode::from_kernel_result(result), None);
    }
    for rip in [0, 1] {
        let mut registers = Registers { rip, rax: 123, ..Registers::default() };
        let before = registers;
        assert_eq!(original.apply(&mut registers, 5), Err(RestartUnderflow));
        assert_eq!(registers, before);
    }
}

#[test]
fn siginfo_preserves_payload_and_fault_address() {
    let mut bytes = [0x5a; 128];
    bytes[..4].copy_from_slice(&64u32.to_le_bytes());
    let info = SigInfo::from_bytes(bytes).unwrap();
    assert_eq!(info.signal(), sig(64));
    assert_eq!(*info.bytes(), bytes);
    for number in [0u32, 65, 0x100_0001] {
        bytes[..4].copy_from_slice(&number.to_le_bytes());
        assert_eq!(SigInfo::from_bytes(bytes), None);
    }
    let fault = SigInfo::fault(sig(11), 2, 0x1234_abcd_9876);
    assert_eq!(&fault.bytes()[..16], &[11, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(&fault.bytes()[16..24], &0x1234_abcd_9876u64.to_le_bytes());
    assert!(fault.bytes()[24..].iter().all(|byte| *byte == 0));
}
