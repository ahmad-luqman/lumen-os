use linux_raw_sys::{errno, general as g};
use lumen_signal::frame::Registers;
use lumen_signal::pending::*;
use lumen_signal::restart::*;
use lumen_signal::{Blocked, DefaultAction, SigSet, Signal};

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
fn siginfo_preserves_payload_and_fault_address() {
    let fault = SigInfo::fault(sig(11), 2, 0x1234_abcd_9876);
    assert_eq!(&fault.bytes()[..16], &[11, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(&fault.bytes()[16..24], &0x1234_abcd_9876u64.to_le_bytes());
    assert!(fault.bytes()[24..].iter().all(|byte| *byte == 0));
}

fn send(p: &mut PendingSignals, target: Target, number: u32, code: i32) -> Result<Enqueued, PendingError> {
    p.enqueue(target, info(number, code), SenderClass::Other)
}
fn take(p: &mut PendingSignals, thread: ThreadSlot) -> Option<SigInfo> {
    p.dequeue(thread, Blocked::EMPTY).unwrap()
}
fn drain(p: &mut PendingSignals, thread: ThreadSlot) -> Vec<SigInfo> {
    let mut values = Vec::new();
    while let Some(value) = take(p, thread) {
        values.push(value);
    }
    values
}

fn drain_threads(p: &mut PendingSignals, slots: &[ThreadSlot]) -> Vec<SigInfo> {
    slots.iter().flat_map(|&slot| drain(p, slot)).collect()
}

#[test]
fn blocked_constructors_and_named_signal_sets() {
    let standard = [
        Signal::HUP,
        Signal::INT,
        Signal::QUIT,
        Signal::ILL,
        Signal::TRAP,
        Signal::ABRT,
        Signal::BUS,
        Signal::FPE,
        Signal::KILL,
        Signal::USR1,
        Signal::SEGV,
        Signal::USR2,
        Signal::PIPE,
        Signal::ALRM,
        Signal::TERM,
        Signal::STKFLT,
        Signal::CHLD,
        Signal::CONT,
        Signal::STOP,
        Signal::TSTP,
        Signal::TTIN,
        Signal::TTOU,
        Signal::URG,
        Signal::XCPU,
        Signal::XFSZ,
        Signal::VTALRM,
        Signal::PROF,
        Signal::WINCH,
        Signal::IO,
        Signal::PWR,
        Signal::SYS,
    ];
    for (index, signal) in standard.into_iter().enumerate() {
        assert_eq!(signal.number(), index as u32 + 1);
    }
    assert_eq!(Signal::RTMIN.number(), 32);
    assert_eq!(Signal::RTMAX.number(), 64);
    const SET: SigSet = SigSet::of(&[Signal::USR1, Signal::KILL, Signal::STOP, Signal::RTMAX, Signal::USR1]);
    assert_eq!(SET.0, (1 << 9) | (1 << 8) | (1 << 18) | (1 << 63));
    assert_eq!(Signal::RTMAX.index(), 63);
    assert_eq!(Signal::RTMAX.bit(), 1 << 63);
    for number in 1..=64 {
        assert_eq!(sig(number).bit(), 1 << (number - 1));
    }
    assert_eq!(Blocked::default(), Blocked::EMPTY);
    let mask = Blocked::new(SET);
    assert_eq!(mask.set(), SigSet::of(&[Signal::USR1, Signal::RTMAX]));
    for op in [g::SIG_BLOCK, g::SIG_UNBLOCK, g::SIG_SETMASK] {
        let updated = mask.apply(op, SigSet::ALL).unwrap();
        assert!(!updated.contains(Signal::KILL));
        assert!(!updated.contains(Signal::STOP));
    }
    assert_eq!(mask.apply(3, SET), Err(-22));
}

#[test]
fn standard_coalescing_and_bounded_realtime_fifo() {
    let (mut p, slots) = PendingSignals::new(1, 3).unwrap();
    assert_eq!(send(&mut p, Target::Process, 10, 101), Ok(Enqueued::Queued));
    assert_eq!(send(&mut p, Target::Process, 10, 202), Ok(Enqueued::Coalesced));
    for (n, code) in [(64, 1), (32, 2), (64, 3)] {
        send(&mut p, Target::Process, n, code).unwrap();
    }
    assert_eq!(send(&mut p, Target::Process, 33, 4), Err(PendingError::Full));
    assert!(!p.pending(slots[0]).unwrap().contains(sig(33)));
    assert_eq!(p.dequeue(slots[0], Blocked::new(SigSet::of(&[Signal::USR1]))), Ok(Some(info(32, 2))));
    assert_eq!(send(&mut p, Target::Process, 32, 5), Ok(Enqueued::Queued));
    assert_eq!(drain(&mut p, slots[0]), [info(10, 101), info(32, 5), info(64, 1), info(64, 3)]);
    assert_eq!(p.pending(slots[0]), Ok(SigSet::EMPTY));
}

#[test]
fn dequeue_prioritizes_synchronous_signals() {
    let (mut p, slots) = PendingSignals::new(1, 33).unwrap();
    // Generation cancels stop/continue, so test CONT separately from the stops.
    for n in (1..=64).rev().filter(|n| *n != 18) {
        send(&mut p, Target::Process, n, n as i32).unwrap();
    }
    let synchronous = [4, 5, 7, 8, 11, 31];
    let order = synchronous.into_iter().chain((1..=64).filter(|n| *n != 18 && !synchronous.contains(n)));
    for n in order {
        assert_eq!(take(&mut p, slots[0]), Some(info(n, n as i32)));
    }
    assert_eq!(take(&mut p, slots[0]), None);
    for n in [1, 4, 9, 19] {
        send(&mut p, Target::Process, n, 0).unwrap();
    }
    let blocked = Blocked::new(SigSet::ALL);
    assert_eq!(p.dequeue(slots[0], blocked), Ok(Some(info(9, 0))));
    assert_eq!(p.dequeue(slots[0], blocked), Ok(Some(info(19, 0))));
    assert_eq!(p.dequeue(slots[0], blocked), Ok(None));
    assert_eq!(p.dequeue(slots[0], Blocked::new(SigSet::of(&[Signal::ILL]))), Ok(Some(info(1, 0))));
    assert_eq!(take(&mut p, slots[0]), Some(info(4, 0)));
}

#[test]
fn sigwait_never_consumes_kill_or_stop() {
    for private in [false, true] {
        let (mut p, slots) = PendingSignals::new(1, 0).unwrap();
        let target = if private { Target::Thread(slots[0]) } else { Target::Process };
        for n in [9, 19, 10, 12] {
            send(&mut p, target, n, 0).unwrap();
        }
        assert_eq!(p.dequeue_wanted(slots[0], SigSet::of(&[Signal::USR1])), Ok(Some(info(10, 0))));
        assert_eq!(p.dequeue_wanted(slots[0], SigSet::of(&[Signal::KILL, Signal::STOP])), Ok(None));
        assert_eq!(p.dequeue_wanted(slots[0], SigSet::ALL), Ok(Some(info(12, 0))));
        assert_eq!(p.dequeue_wanted(slots[0], SigSet::ALL), Ok(None));
        assert_eq!(drain(&mut p, slots[0]), [info(9, 0), info(19, 0)]);
    }
}

#[test]
fn private_pending_precedes_shared_and_threads_are_isolated() {
    let (mut p, slots) = PendingSignals::new(2, 2).unwrap();
    send(&mut p, Target::Thread(slots[0]), 10, 1).unwrap();
    send(&mut p, Target::Thread(slots[1]), 12, 2).unwrap();
    send(&mut p, Target::Process, 4, 3).unwrap();
    assert_eq!(drain(&mut p, slots[0]), [info(10, 1), info(4, 3)]);
    assert_eq!(drain(&mut p, slots[1]), [info(12, 2)]);
    send(&mut p, Target::Process, 10, 4).unwrap();
    send(&mut p, Target::Thread(slots[0]), 10, 5).unwrap();
    assert_eq!(drain(&mut p, slots[0]), [info(10, 5), info(10, 4)]);
}

#[test]
fn stop_continue_cancels_across_all_queues() {
    for stop in [19, 20, 21, 22] {
        for index in 0..3 {
            let (mut p, slots) = PendingSignals::new(2, 1).unwrap();
            let targets = [Target::Process, Target::Thread(slots[0]), Target::Thread(slots[1])];
            let target = targets[index];
            send(&mut p, Target::Process, 12, 99).unwrap();
            for destination in targets {
                send(&mut p, destination, 18, 0).unwrap();
            }
            send(&mut p, target, stop, 0).unwrap();
            let mut numbers: Vec<_> = drain_threads(&mut p, &slots).into_iter().map(|i| i.signal().number()).collect();
            numbers.sort();
            assert_eq!(numbers, [12, stop]);
        }
    }
    for index in 0..3 {
        let (mut p, slots) = PendingSignals::new(2, 1).unwrap();
        let targets = [Target::Process, Target::Thread(slots[0]), Target::Thread(slots[1])];
        let target = targets[index];
        for destination in targets {
            for stop in [19, 20, 21, 22] {
                send(&mut p, destination, stop, 0).unwrap();
            }
        }
        send(&mut p, target, 18, 0).unwrap();
        let seen: Vec<_> = drain_threads(&mut p, &slots);
        assert_eq!(seen, [info(18, 0)]);
    }
    // The kernel drops an ignored generation only after these side effects.
    let (mut p, slots) = PendingSignals::new(1, 0).unwrap();
    send(&mut p, Target::Process, 18, 0).unwrap();
    p.prepare_signal(Signal::TSTP);
    assert_eq!(take(&mut p, slots[0]), None);
    send(&mut p, Target::Process, 19, 0).unwrap();
    p.prepare_signal(Signal::CONT);
    assert_eq!(take(&mut p, slots[0]), None);
}

#[test]
fn ignore_discards_pending_and_default_ignore_is_respected() {
    for (number, disposition) in [
        (10, Disposition::Ignore),
        (32, Disposition::Ignore),
        (17, Disposition::Default),
        (18, Disposition::Default),
        (23, Disposition::Default),
        (28, Disposition::Default),
    ] {
        let (mut p, slots) = PendingSignals::new(2, 4).unwrap();
        for destination in [Target::Process, Target::Thread(slots[0]), Target::Thread(slots[1])] {
            for (n, code) in [(1, 77), (number, 1), (number, 2), (64, 88)] {
                send(&mut p, destination, n, code).unwrap();
            }
        }
        p.disposition_changed(sig(number), disposition);
        let kept: Vec<_> = drain_threads(&mut p, &slots);
        assert_eq!(kept, [info(1, 77), info(64, 88), info(1, 77), info(64, 88), info(1, 77), info(64, 88)]);
        send(&mut p, Target::Process, number, 3).unwrap();
        assert_eq!(p.dequeue_wanted(slots[0], SigSet::of(&[sig(number)])), Ok(Some(info(number, 3))));
    }
    for disposition in [Disposition::Default, Disposition::Handler] {
        let (mut p, slots) = PendingSignals::new(1, 0).unwrap();
        send(&mut p, Target::Process, 10, 0).unwrap();
        p.disposition_changed(Signal::USR1, disposition);
        assert_eq!(take(&mut p, slots[0]), Some(info(10, 0)));
    }
}

#[test]
fn queue_caps_sender_fallback_markers_and_errno_mapping() {
    assert_eq!(PendingError::Full.errno(), Some(-11));
    assert_eq!(PendingError::NoMemory.errno(), Some(-11));
    assert_eq!(PendingError::InvalidThread.errno(), None);
    assert!(matches!(PendingSignals::new(usize::MAX, 0), Err(PendingError::NoMemory)));
    for index in 0..3 {
        let (mut p, slots) = PendingSignals::new(2, 1).unwrap();
        let targets = [Target::Process, Target::Thread(slots[0]), Target::Thread(slots[1])];
        let destination = targets[index];
        for target in targets {
            send(&mut p, target, 32, 100).unwrap();
        }
        assert_eq!(send(&mut p, destination, 33, 200), Err(PendingError::Full));
        for n in [32, 33, 33] {
            assert_eq!(p.enqueue(destination, info(n, 1234), SenderClass::SiUser), Ok(Enqueued::InfoLost));
        }
        let items: Vec<_> = drain_threads(&mut p, &slots);
        assert_eq!(items.iter().filter(|&&i| i == info(32, 100)).count(), 3);
        assert_eq!(items.iter().filter(|&&i| i == info(32, 0)).count(), 0);
        assert_eq!(items.iter().filter(|&&i| i == info(33, 0)).count(), 1);
        assert_eq!(items.len(), 4);
        send(&mut p, destination, 34, 300).unwrap();
    }
    let (mut zero, slots) = PendingSignals::new(1, 0).unwrap();
    assert_eq!(send(&mut zero, Target::Process, 32, 1), Err(PendingError::Full));
    zero.enqueue(Target::Process, info(32, 123), SenderClass::SiUser).unwrap();
    zero.disposition_changed(Signal::RTMIN, Disposition::Ignore);
    assert_eq!(take(&mut zero, slots[0]), None);
    send(&mut zero, Target::Process, 1, 0).unwrap();
    assert_eq!(take(&mut zero, slots[0]), Some(info(1, 0)));
}

#[test]
fn every_restart_table_row_and_register_effect() {
    use RestartCode::*;
    use RestartDecision::*;
    let original = Restart { rax: 44 };
    let block = Restart { rax: 219 };
    let rows = [
        (Sys, [original, ReturnEintr, original]),
        (NoIntr, [original; 3]),
        (NoHand, [original, ReturnEintr, ReturnEintr]),
        (RestartBlock, [block, ReturnEintr, ReturnEintr]),
    ];
    for (code, expected) in rows {
        assert_eq!(RestartCode::from_kernel_result(-(code as i64)), Some(code));
        for (handling, decision) in
            [Handling::None, Handling::Handler { sa_restart: false }, Handling::Handler { sa_restart: true }]
                .into_iter()
                .zip(expected)
        {
            let exit = SyscallExit::new(44);
            assert_eq!(decide(exit, (-(code as i64)) as u64, handling), decision);
            let before = Registers { rip: 0x400102, rax: (-(code as i64)) as u64, rbx: 777, ..Registers::default() };
            let mut actual = before;
            decision.apply(&mut actual).unwrap();
            let mut wanted = before;
            match decision {
                ReturnEintr => wanted.rax = (-4i64) as u64,
                Restart { rax } => {
                    wanted.rip = 0x400100;
                    wanted.rax = rax;
                }
                Unchanged => unreachable!(),
            }
            assert_eq!(actual, wanted);
            assert_eq!(decide(None, (-(code as i64)) as u64, handling), Unchanged);
            let mut async_regs = before;
            decide(None, (-(code as i64)) as u64, handling).apply(&mut async_regs).unwrap();
            assert_eq!(async_regs, before);
        }
    }
    for result in [0, 512, -1, -4, -515, i64::MIN] {
        assert_eq!(RestartCode::from_kernel_result(result), None);
        assert_eq!(decide(SyscallExit::new(44), result as u64, Handling::None), Unchanged);
    }
    assert_eq!(SyscallExit::new(u64::MAX), None);
    assert_eq!(SyscallExit::new(44).unwrap().orig_rax(), 44);
    for rip in [0, 1] {
        let mut registers = Registers { rip, rax: 123, ..Registers::default() };
        let before = registers;
        assert_eq!(original.apply(&mut registers), Err(RestartUnderflow));
        assert_eq!(registers, before);
    }
}

#[test]
fn user_siginfo_stamps_syscall_signal() {
    for number in 1..=64 {
        let bytes = [0xff; 128];
        let info = SigInfo::from_user(sig(number), bytes);
        assert_eq!(info.signal(), sig(number));
        assert_eq!(&info.bytes()[..4], &number.to_le_bytes());
        assert_eq!(&info.bytes()[4..48], &bytes[4..48]);
        assert_eq!(&info.bytes()[48..], &[0; 80]);
    }
}

#[test]
fn thread_lifecycle_pending_union_and_deliverability() {
    let (mut p, initial) = PendingSignals::new(0, 1).unwrap();
    assert!(initial.is_empty());
    let first = p.add_thread().unwrap();
    let second = p.add_thread().unwrap();
    assert_ne!(first, second);
    send(&mut p, Target::Thread(first), 10, 1).unwrap();
    assert_eq!(p.has_deliverable(first, Blocked::EMPTY), Ok(true));
    assert_eq!(p.has_deliverable(first, Blocked::new(SigSet::of(&[Signal::USR1]))), Ok(false));
    assert_eq!(p.has_deliverable(second, Blocked::EMPTY), Ok(false));
    send(&mut p, Target::Thread(second), 12, 2).unwrap();
    send(&mut p, Target::Process, 32, 3).unwrap();
    assert_eq!(p.pending(first), Ok(SigSet::of(&[Signal::USR1, Signal::RTMIN])));
    assert_eq!(p.pending(second), Ok(SigSet::of(&[Signal::USR2, Signal::RTMIN])));
    send(&mut p, Target::Process, 9, 4).unwrap();
    assert_eq!(p.has_deliverable(first, Blocked::new(SigSet::ALL)), Ok(true));
    p.remove_thread(first).unwrap();
    assert_eq!(p.pending(first), Err(PendingError::InvalidThread));
    assert_eq!(p.has_deliverable(first, Blocked::EMPTY), Err(PendingError::InvalidThread));
    assert_eq!(p.dequeue(first, Blocked::EMPTY), Err(PendingError::InvalidThread));
    assert_eq!(p.dequeue_wanted(first, SigSet::ALL), Err(PendingError::InvalidThread));
    assert_eq!(p.remove_thread(first), Err(PendingError::InvalidThread));
    assert_eq!(send(&mut p, Target::Thread(first), 10, 0), Err(PendingError::InvalidThread));
    let added = p.add_thread().unwrap();
    assert_ne!(added, first);
    assert_eq!(drain(&mut p, second), [info(12, 2), info(9, 4), info(32, 3)]);
    assert_eq!(p.pending(added), Ok(SigSet::EMPTY));
    send(&mut p, Target::Thread(added), 32, 1).unwrap();
    assert_eq!(send(&mut p, Target::Thread(added), 33, 2), Err(PendingError::Full));
    // Validate the destination before prepare_signal could cancel CONT.
    send(&mut p, Target::Process, 18, 99).unwrap();
    assert_eq!(send(&mut p, Target::Thread(first), 19, 0), Err(PendingError::InvalidThread));
    assert_eq!(p.dequeue_wanted(added, SigSet::of(&[Signal::CONT])), Ok(Some(info(18, 99))));
}

#[test]
fn target_selection_checks_masks_states_and_slot_identity() {
    use ThreadState::*;
    let (mut p, slots) = PendingSignals::new(3, 0).unwrap();
    for state in [Running, Sleeping, Stopped, Traced, Exiting] {
        for has_pending in [false, true] {
            for set in [SigSet::EMPTY, SigSet::ALL] {
                let threads = [ThreadCandidate { slot: slots[0], state, mask: Blocked::new(set), has_pending }];
                let ordinary = set == SigSet::EMPTY && (state == Running || (state == Sleeping && !has_pending));
                assert_eq!(select_target(sig(10), &threads, slots[0]), Ok(ordinary.then_some(slots[0])));
                assert_eq!(select_target(sig(9), &threads, slots[0]), Ok((state != Exiting).then_some(slots[0])));
            }
        }
    }
    let running = ThreadCandidate { slot: slots[0], state: Running, mask: Blocked::EMPTY, has_pending: true };
    assert_eq!(
        select_target(sig(19), &[ThreadCandidate { mask: Blocked::new(SigSet::ALL), ..running }], slots[0]),
        Ok(Some(slots[0]))
    );
    p.remove_thread(slots[0]).unwrap();
    // Candidate positions [0, 1] now represent slots [2, 1], not indices [0, 1].
    let threads = [ThreadCandidate { slot: slots[2], ..running }, ThreadCandidate { slot: slots[1], ..running }];
    assert_eq!(select_target(sig(10), &threads, slots[1]), Ok(Some(slots[1])));
    let chosen = select_target(sig(10), &threads, slots[2]).unwrap().unwrap();
    p.enqueue(Target::Thread(chosen), info(10, 8), SenderClass::Other).unwrap();
    assert_eq!(take(&mut p, slots[1]), None);
    assert_eq!(take(&mut p, slots[2]), Some(info(10, 8)));
    let blocked = [ThreadCandidate { mask: Blocked::new(SigSet::ALL), ..threads[0] }, threads[1]];
    assert_eq!(select_target(sig(10), &blocked, slots[2]), Ok(Some(slots[1])));
    assert_eq!(select_target(sig(10), &threads, slots[0]), Err(BadCursor));
    assert_eq!(select_target(sig(10), &[], slots[1]), Err(BadCursor));
}

#[test]
fn overflow_bit_discard_and_reenqueue_delivers_only_once() {
    let (mut p, slots) = PendingSignals::new(1, 1).unwrap();
    send(&mut p, Target::Process, 33, 7).unwrap();
    p.enqueue(Target::Process, info(32, 9), SenderClass::SiUser).unwrap();
    p.disposition_changed(Signal::RTMIN, Disposition::Ignore);
    assert_eq!(drain(&mut p, slots[0]), [info(33, 7)]);
    send(&mut p, Target::Process, 32, 8).unwrap();
    assert_eq!(drain(&mut p, slots[0]), [info(32, 8)]);
    // With an existing entry, overflow does not add another delivery.
    send(&mut p, Target::Process, 32, 10).unwrap();
    p.enqueue(Target::Process, info(32, 11), SenderClass::SiUser).unwrap();
    assert_eq!(drain(&mut p, slots[0]), [info(32, 10)]);
}

#[test]
fn handlers_keep_default_ignored_signals_and_unblockables_never_flush() {
    let (mut p, slots) = PendingSignals::new(1, 0).unwrap();
    for signal in [Signal::CHLD, Signal::URG, Signal::WINCH, Signal::CONT] {
        let item = SigInfo::new(signal, 7);
        p.enqueue(Target::Thread(slots[0]), item, SenderClass::Other).unwrap();
        p.disposition_changed(signal, Disposition::Handler);
        assert_eq!(take(&mut p, slots[0]), Some(item));
    }
    for signal in [Signal::KILL, Signal::STOP] {
        for disposition in [Disposition::Default, Disposition::Ignore, Disposition::Handler] {
            let item = SigInfo::new(signal, 8);
            p.enqueue(Target::Process, item, SenderClass::Other).unwrap();
            p.disposition_changed(signal, disposition);
            assert_eq!(take(&mut p, slots[0]), Some(item));
        }
    }
}
