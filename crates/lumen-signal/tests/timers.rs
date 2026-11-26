use lumen_signal::pending::{Enqueued, PendingSignals, SenderClass, SigInfo, Target};
use lumen_signal::{Blocked, SigSet, Signal};

#[test]
fn timer_predicates_cover_all_queues_without_discarding_other_senders() {
    let (mut p, slots) = PendingSignals::new(2, 8).unwrap();
    for target in [Target::Process, Target::Thread(slots[0]), Target::Thread(slots[1])] {
        for info in [
            SigInfo::timer(Signal::ALRM, 7, 123),
            SigInfo::timer(Signal::RTMIN, 7, 123),
            SigInfo::timer(Signal::RTMIN, 8, 456),
            SigInfo::new(Signal::USR1, 0),
        ] {
            assert_eq!(p.enqueue(target, info, SenderClass::Other), Ok(Enqueued::Queued));
        }
    }
    assert!(p.any(|i| i.timer_id() == Some(7)));
    p.discard_if(|i| i.timer_id() == Some(7));
    assert!(!p.any(|i| i.timer_id() == Some(7)));
    for &slot in &slots {
        assert_eq!(p.pending(slot).unwrap(), SigSet::of(&[Signal::USR1, Signal::RTMIN]));
    }
    let mut got = Vec::new();
    for &slot in &slots {
        while let Some(info) = p.dequeue(slot, Blocked::EMPTY).unwrap() {
            got.push(info);
        }
    }
    assert_eq!(got.iter().filter(|i| i.timer_id() == Some(8)).count(), 3);
    assert_eq!(got.iter().filter(|i| i.timer_id().is_none()).count(), 3);
    assert!(!p.any(|_| true));
}

#[test]
fn predicate_removal_preserves_fifo_and_overflow_only_bits() {
    let (mut p, slots) = PendingSignals::new(1, 3).unwrap();
    for id in [1, 2, 3] {
        p.enqueue(Target::Process, SigInfo::timer(Signal::RTMIN, id, 0), SenderClass::Other).unwrap();
    }
    p.enqueue(Target::Process, SigInfo::new(Signal::RTMAX, 0), SenderClass::SiUser).unwrap();
    p.discard_if(|i| i.timer_id() == Some(2));
    assert!(p.pending(slots[0]).unwrap().contains(Signal::RTMAX));
    for id in [1, 3] {
        assert_eq!(p.dequeue(slots[0], Blocked::EMPTY).unwrap().unwrap().timer_id(), Some(id));
    }
    assert_eq!(p.dequeue(slots[0], Blocked::EMPTY).unwrap().unwrap().signal(), Signal::RTMAX);
    assert_eq!(p.pending(slots[0]).unwrap(), SigSet::EMPTY);
}

#[test]
fn timer_siginfo_uses_linux_payload_offsets() {
    let mut info = SigInfo::timer(Signal::RTMAX, 31, 0x123456789abcdef0);
    info.set_timer_overrun(123);
    assert_eq!(info.timer_id(), Some(31));
    assert_eq!(i32::from_le_bytes(info.bytes()[8..12].try_into().unwrap()), -2);
    assert_eq!(i32::from_le_bytes(info.bytes()[20..24].try_into().unwrap()), 123);
    assert_eq!(u64::from_le_bytes(info.bytes()[24..32].try_into().unwrap()), 0x123456789abcdef0);
    assert!(info.bytes()[32..].iter().all(|&b| b == 0));
    assert_eq!(SigInfo::new(Signal::ALRM, 0).timer_id(), None);
}

#[test]
fn predicates_find_private_only_and_skip_vacant_queues() {
    let (mut p, slots) = PendingSignals::new(2, 1).unwrap();
    p.remove_thread(slots[0]).unwrap();
    p.enqueue(Target::Thread(slots[1]), SigInfo::timer(Signal::RTMIN, 3, 0), SenderClass::Other).unwrap();
    assert!(p.any(|i| i.timer_id() == Some(3)));
    assert!(!p.any(|i| i.timer_id() == Some(4)));
    p.discard_if(|i| i.timer_id() == Some(3));
    assert!(!p.any(|_| true));
    assert_eq!(p.pending(slots[1]).unwrap(), SigSet::EMPTY);
}

#[test]
fn any_finds_standard_only_match() {
    for private in [false, true] {
        let (mut p, slots) = PendingSignals::new(1, 1).unwrap();
        let target = if private { Target::Thread(slots[0]) } else { Target::Process };
        p.enqueue(target, SigInfo::new(Signal::ALRM, 0), SenderClass::Other).unwrap();
        assert!(p.any(|i| i.signal() == Signal::ALRM), "standard-only match missing");
        assert!(!p.any(|i| i.signal() == Signal::RTMIN));
    }
}

#[test]
fn any_finds_shared_only_match() {
    for signal in [Signal::ALRM, Signal::RTMIN] {
        let (mut p, _) = PendingSignals::new(1, 1).unwrap();
        p.enqueue(Target::Process, SigInfo::new(signal, 0), SenderClass::Other).unwrap();
        assert!(p.any(|i| i.signal() == signal), "shared-only match missing");
    }
}

#[test]
fn timer_provenance_cannot_be_forged_by_siginfo() {
    for signal in [Signal::ALRM, Signal::RTMIN] {
        let (mut p, slots) = PendingSignals::new(1, 4).unwrap();
        let info = SigInfo::timer(signal, 7, 123);
        p.enqueue(Target::Process, SigInfo::from_user(signal, *info.bytes()), SenderClass::Other).unwrap();
        assert!(!p.any_timer(99));
        p.discard_timer(99);
        p.discard_timers();
        let fake = p.dequeue_entry(slots[0], Blocked::EMPTY).unwrap().unwrap();
        assert_eq!(fake.info(), info);
        assert_eq!(fake.timer_instance(), None);
        p.enqueue_timer(Target::Process, info, 99).unwrap();
        assert!(p.any_timer(99));
        assert!(!p.any_timer(100));
        let real = p.dequeue_entry(slots[0], Blocked::EMPTY).unwrap().unwrap();
        assert_eq!(real.info(), info);
        assert_eq!(real.timer_instance(), Some(99));
        p.enqueue_timer(Target::Thread(slots[0]), info, 99).unwrap();
        p.discard_timer(100);
        assert!(p.any_timer(99));
        p.discard_timer(99);
        assert!(!p.any(|_| true));
    }
}

#[test]
fn exec_discards_trusted_timers_from_each_queue_and_keeps_user_entries() {
    let (mut pending, slots) = PendingSignals::new(3, 8).unwrap();
    pending.remove_thread(slots[1]).unwrap();
    for target in [Target::Process, Target::Thread(slots[0]), Target::Thread(slots[2])] {
        pending.enqueue_timer(target, SigInfo::timer(Signal::ALRM, 7, 11), 99).unwrap();
        pending.enqueue_timer(target, SigInfo::timer(Signal::RTMIN, 7, 22), 99).unwrap();
        for signal in [Signal::USR1, Signal::RTMIN] {
            let fake = SigInfo::timer(signal, 7, 33);
            pending.enqueue(target, SigInfo::from_user(signal, *fake.bytes()), SenderClass::Other).unwrap();
        }
    }
    pending.discard_timers();
    assert!(!pending.any_timer(99), "exec retained a trusted timer notification");
    for &slot in &[slots[0], slots[2]] {
        assert_eq!(pending.pending(slot).unwrap(), SigSet::of(&[Signal::USR1, Signal::RTMIN]));
    }
    let mut retained = 0;
    for &slot in &[slots[0], slots[2]] {
        while let Some(entry) = pending.dequeue_entry(slot, Blocked::EMPTY).unwrap() {
            assert_eq!(entry.timer_instance(), None);
            assert_eq!(entry.info(), SigInfo::timer(entry.info().signal(), 7, 33));
            retained += 1;
        }
    }
    assert_eq!(retained, 6, "exec lost a user-origin entry");
    assert!(!pending.any(|_| true));
}
