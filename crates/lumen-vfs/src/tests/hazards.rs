//! M8's shared-description and descriptor-reservation rules.

use super::*;

#[test]
fn reservations_are_neither_open_nor_free() {
    let mut t = FdTable::new();
    let r = t.reserve_below(MAX_FDS).unwrap();
    assert_eq!(r.fd(), 0);
    assert_eq!(t.reserved_count(), 1);
    assert!(t.get(0).is_none());
    assert!(t.get_mut(0).is_none());
    assert_eq!(t.remove(0).unwrap_err(), VfsError::BadF);
    assert_eq!(t.get_cloexec(0), Err(VfsError::BadF));
    assert_eq!(t.set_cloexec(0, true), Err(VfsError::BadF));
    assert_eq!(t.insert_at(0, 99, false).unwrap_err(), VfsError::Busy);
    assert_eq!(t.open_count(), 0);
    assert_eq!(t.iter_open().count(), 0);
    assert_eq!(t.alloc_below(MAX_FDS, 10, true), Ok(1));
    assert_eq!(t.alloc_from_below(0, MAX_FDS, 20, false), Ok(2));
    let mut fork = t.clone();
    assert_eq!(fork.reserved_count(), 0);
    assert_eq!(fork.alloc_below(MAX_FDS, 30, false), Ok(0));
    assert_eq!(t.drain_cloexec().len(), 1);
    assert_eq!(t.drain_all().len(), 1);
    assert_eq!(t.reserved_count(), 1);
    // Drains do not steal or cancel the in-flight open.
    assert_eq!(t.alloc_below(MAX_FDS, 40, false), Ok(1));
    assert_eq!(t.install(r, 50, true), 0);
    assert_eq!(t.reserved_count(), 0);
    assert_eq!(t.get(0).unwrap().file, 50);
    assert_eq!(t.get_cloexec(0), Ok(true));
    let r = t.reserve_below(MAX_FDS).unwrap();
    assert_eq!(r.fd(), 2);
    t.cancel(r);
    assert_eq!(t.reserved_count(), 0);
    assert_eq!(t.alloc_below(MAX_FDS, 60, false), Ok(2));
}

#[test]
fn reservations_fill_the_table_and_cancel_reuses_the_lowest_slot() {
    let mut t = FdTable::<u32>::new();
    assert_eq!(t.reserve_below(0).unwrap_err(), VfsError::MFile);
    let mut held = Vec::new();
    for fd in 0..MAX_FDS {
        let r = t.reserve_below(MAX_FDS).unwrap();
        assert_eq!(r.fd(), fd as i32);
        held.push(r);
    }
    assert_eq!(t.open_count(), 0);
    assert_eq!(t.reserved_count(), MAX_FDS);
    assert_eq!(t.reserve_below(MAX_FDS).unwrap_err(), VfsError::MFile);
    assert_eq!(t.alloc_below(MAX_FDS, 1, false), Err(VfsError::MFile));
    let r = held.remove(500);
    t.cancel(r);
    assert_eq!(t.alloc_from_below(501, MAX_FDS, 1, false), Err(VfsError::MFile));
    let r = t.reserve_below(MAX_FDS).unwrap();
    assert_eq!(r.fd(), 500);
    assert_eq!(t.install(r, 77, false), 500);
    for r in held {
        t.cancel(r);
    }
    assert_eq!(t.alloc_below(MAX_FDS, 1, false), Ok(0));
    assert_eq!(t.get(500).unwrap().file, 77);
}

#[test]
fn reservation_cannot_install_in_a_different_table() {
    let mut a = FdTable::<u32>::new();
    let mut b = FdTable::new();
    let ra = a.reserve_below(MAX_FDS).unwrap();
    let rb = b.reserve_below(MAX_FDS).unwrap();
    let refused = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        b.install(ra, 1, false);
    }));
    assert!(refused.is_err());
    assert_eq!(b.install(rb, 2, false), 0);
    assert_eq!(b.get(0).unwrap().file, 2);
}

#[test]
fn dropped_reservation_stays_reserved_for_the_table_lifetime() {
    let mut t = FdTable::<u32>::new();
    {
        let _r = t.reserve_below(MAX_FDS).unwrap();
    }
    assert_eq!(t.reserved_count(), 1);
    assert_eq!(t.open_count(), 0);
    assert!(t.drain_all().is_empty());
    assert_eq!(t.alloc_below(MAX_FDS, 1, false), Ok(1));
    assert_eq!(t.insert_at(0, 2, false).unwrap_err(), VfsError::Busy);
}

#[test]
fn posix_wait_graph_keeps_threads_and_follows_the_lowest_live_tid() {
    let mut g = PosixWaitGraph::new();
    // Insert the higher tid first: selection is by tid, not insertion.
    assert_eq!(g.wait_for(Waiter { tgid: 1, tid: 19 }, 3, 0, 0), Ok(WaitSearch::Clear));
    assert_eq!(g.wait_for(Waiter { tgid: 1, tid: 11 }, 2, 0, 0), Ok(WaitSearch::Clear));
    assert!(g.is_recorded(Waiter { tgid: 1, tid: 11 }) && g.is_recorded(Waiter { tgid: 1, tid: 19 }));
    assert_eq!(g.wait_for(Waiter { tgid: 2, tid: 20 }, 1, 0, 0), Err(Deadlock));
    // Only the selected thread's blocker is followed (the documented
    // Linux approximation, not a search of all a process's edges).
    assert_eq!(g.wait_for(Waiter { tgid: 3, tid: 30 }, 1, 0, 0), Ok(WaitSearch::Clear));
    g.done(Waiter { tgid: 3, tid: 30 });
    g.done(Waiter { tgid: 1, tid: 11 });
    assert!(g.is_recorded(Waiter { tgid: 1, tid: 19 }));
    assert_eq!(g.wait_for(Waiter { tgid: 3, tid: 30 }, 1, 0, 0), Err(Deadlock));
    // A reused tid in a different group is a distinct key.
    assert_eq!(g.wait_for(Waiter { tgid: 4, tid: 19 }, 5, 0, 0), Ok(WaitSearch::Clear));
    g.done(Waiter { tgid: 4, tid: 19 });
    assert!(g.is_recorded(Waiter { tgid: 1, tid: 19 }));
    // Stale lower tids are no longer waiting: use the lowest live one.
    assert_eq!(g.wait_for(Waiter { tgid: 1, tid: 11 }, 2, 0, 0), Ok(WaitSearch::Clear));
    assert_eq!(g.wait_for(Waiter { tgid: 1, tid: 19 }, 3, 1, 1), Ok(WaitSearch::Clear));
    assert_eq!(g.wait_for(Waiter { tgid: 3, tid: 30 }, 1, 1, 1), Err(Deadlock));
    assert_eq!(g.wait_for(Waiter { tgid: 1, tid: 11 }, 2, 0, 1), Ok(WaitSearch::StaleCaller));
    assert!(!g.is_recorded(Waiter { tgid: 1, tid: 11 }) && g.is_recorded(Waiter { tgid: 1, tid: 19 }));
}

#[test]
fn open_file_position_serializes_offset_updates() {
    let (env, _) = setup_vfs();
    let (inode, _) =
        open_inode(&env, None, b"/position", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    inode.write_at(0, b"abcd").unwrap();
    let file = OpenFile::<StdLock<u64>>::new_unchecked_flags(
        inode,
        Arc::from(&b"/position"[..]),
        OpenFlags(OpenFlags::O_RDWR),
    )
    .unwrap();
    // The position lock is separate from Lock 2: snapshot and give-back
    // can release and reacquire the latter while retaining exclusivity.
    let pos = file.lock_pos().expect("regular file position");
    assert!(file.pos.0.try_lock().is_err());
    *file.offset.lock() = 2;
    *file.offset.lock() = 0;
    drop(pos);
    assert!(file.pos.0.try_lock().is_ok());
    let reads = std::thread::scope(|s| {
        let read = || {
            let mut out = Vec::new();
            loop {
                let _pos = file.lock_pos();
                let at = *file.offset.lock();
                std::thread::yield_now();
                let mut byte = [0];
                if file.inode().read_at(at, &mut byte).unwrap() == 0 {
                    break;
                }
                *file.offset.lock() = at + 1;
                out.push(byte[0]);
            }
            out
        };
        let a = s.spawn(read);
        let b = s.spawn(read);
        [a.join().unwrap(), b.join().unwrap()].concat()
    });
    let mut reads = reads;
    reads.sort_unstable();
    assert_eq!(reads, b"abcd");
    assert_eq!(*file.offset.lock(), 4);
    let root = resolve_path(&env, None, b"/", ResolveFlags::default()).unwrap().inode;
    let dir = OpenFile::<StdLock<u64>>::new_unchecked_flags(root, Arc::from(&b"/"[..]), OpenFlags(OpenFlags::O_RDONLY))
        .unwrap();
    assert!(dir.lock_pos().is_some());
}

#[test]
fn open_file_streams_do_not_take_position_lock() {
    for kind in [InodeType::CharDevice, InodeType::BlockDevice, InodeType::Fifo, InodeType::Socket] {
        let inode = MemInode::new_special(
            DevId::DEVFS,
            1,
            kind,
            0o600,
            DevId(0),
            Arc::new(Mutex::new(BTreeMap::new())),
            Arc::new(AtomicU64::new(2)),
        );
        let file = OpenFile::<StdLock<u64>>::new_unchecked_flags(
            inode,
            Arc::from(&b"/stream"[..]),
            OpenFlags(OpenFlags::O_RDWR),
        )
        .unwrap();
        assert!(file.lock_pos().is_none(), "{kind:?}");
    }
}
