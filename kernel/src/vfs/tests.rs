//! The vfs ktests: the `ext4plus` smoke check, the ext4 adapter over the
//! The ktests of `vfs/mod.rs`'s code (`vfs/ext4.rs` and `vfs/page_cache.rs`
//! keep their own): the `ext4plus` smoke check, the ext4 adapter over the
//! page cache, `/tmp`, `/dev` and `/proc`, tmpfs's quota, `sync`, global
//! reclaim and recovery from injected I/O errors.
//!
//! `MemBlkDev` stays in `vfs/mod.rs`: the `vfs/ext4.rs` and
//! `vfs/page_cache.rs` ktests use it too.

use super::*;
use lumen_vfs::OpenGuard;

crate::ktest!(
    fn ext4_crate_smoke() {
        let stats = exercise_ext4_smoke()?;
        crate::kassert_eq!(stats.blocks_count, IMAGE_BLOCKS as u64);
        crate::kassert_eq!(stats.free_blocks_after, u64::from(FREE_BLOCKS));
        crate::kassert_eq!(stats.free_inodes_after, FREE_INODES - 1);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_adapter_pagecache_quirks_and_open_unlinked() {
        use crate::{kassert, kassert_eq};
        use lumen_bcache::BlockCache;
        use lumen_vfs::OpenFile;

        let img = build_minimal_ext4_image_blocks(256);
        let mem_dev = MemBlkDev::new(img, false);
        let bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(mem_dev.clone(), 64).map_err(|_| "bcache init")?));
        let fs =
            ext4::Ext4FileSystem::mount(Arc::clone(&bcache), DevId::makedev(0, 10), false).map_err(|_| "ext4 mount")?;

        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;

        // 1. Write-back PageCache + OpenFile<KernelVfsLock<u64>> + truncate shrink tail zeroing + sync.
        let (f, path_str) = open_inode(
            &tables,
            None,
            b"/tail.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create tail.bin")?;
        let of = OpenFile::<KernelVfsLock<u64>>::new_unchecked_flags(
            Arc::clone(&f),
            Arc::from(path_str.as_str()),
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR),
        )
        .map_err(|_| "OpenFile::new_unchecked_flags")?;
        *of.offset.lock() = 123;
        kassert_eq!(*of.offset.lock(), 123);
        drop(of);

        let mut payload = vec![0xAAu8; 5000];
        payload[..10].copy_from_slice(b"0123456789");
        f.write_at(0, &payload).map_err(|_| "write tail.bin")?;
        f.sync(false).map_err(|_| "sync tail.bin")?;
        kassert!(mem_dev.0.flushed.load(core::sync::atomic::Ordering::Acquire));

        // Shrink to 100 (partial page 0) -> tail [100..4096) zeroed on disk too.
        f.truncate(100).map_err(|_| "truncate shrink")?;
        f.truncate(200).map_err(|_| "truncate extend")?;
        let mut check = [0xFFu8; 200];
        kassert_eq!(f.read_at(0, &mut check).map_err(|_| "read after extend")?, 200);
        kassert_eq!(&check[..10], b"0123456789");
        kassert!(check[100..200].iter().all(|&b| b == 0));

        // 2. Inline symlink <= 59B, 60B, and >= 60B block symlink + unlink inline symlink
        //    + hard-linked inline symlink unlink preserves surviving link.
        let target60 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab";
        kassert_eq!(target60.len(), 60);
        let _ = symlink_at(&tables, target60.as_bytes(), None, b"/sym60").map_err(|_| "sym60")?;
        kassert_eq!(readlink_at(&tables, None, b"/sym60").map_err(|_| "readlink sym60")?, target60.as_bytes());
        let _ = symlink_at(&tables, b"/tail.bin", None, b"/sym_short").map_err(|_| "sym_short")?;
        kassert_eq!(readlink_at(&tables, None, b"/sym_short").map_err(|_| "readlink sym_short")?, b"/tail.bin");
        unlink_at(&tables, None, b"/sym_short", false).map_err(|_| "unlink inline symlink")?;

        let _ = symlink_at(&tables, b"/tail.bin", None, b"/sym_hl_1").map_err(|_| "sym_hl_1")?;
        lumen_vfs::link_at(&tables, None, b"/sym_hl_1", None, b"/sym_hl_2", false, false)
            .map_err(|_| "link inline symlink")?;
        unlink_at(&tables, None, b"/sym_hl_1", false).map_err(|_| "unlink first inline symlink link")?;
        kassert_eq!(
            readlink_at(&tables, None, b"/sym_hl_2").map_err(|_| "readlink surviving inline symlink")?,
            b"/tail.bin"
        );
        unlink_at(&tables, None, b"/sym_hl_2", false).map_err(|_| "unlink second inline symlink link")?;
        kassert!(matches!(readlink_at(&tables, None, b"/sym_hl_2"), Err(VfsError::NoEnt)));

        // 3. Gate 6 open-unlinked contract on ext4 file & directory.
        let g = OpenGuard::new_unchecked(Arc::clone(&f));
        let orig_ino = f.ino();
        unlink_at(&tables, None, b"/tail.bin", false).map_err(|_| "open unlink tail.bin")?;
        kassert!(f.is_detached());
        kassert_ne_u64(f.ino(), orig_ino)?;
        kassert_eq!(f.stat().map_err(|_| "stat detached")?.nlink, 0);
        let mut after_unlink = [0u8; 10];
        kassert_eq!(f.read_at(0, &mut after_unlink).map_err(|_| "read detached")?, 10);
        kassert_eq!(&after_unlink, b"0123456789");
        f.sync(false).map_err(|_| "sync detached")?;
        drop(g);
        drop(f);

        // 4. Cross-dir mkdir + rename (0, NOREPLACE, EXCHANGE) updating `..`.
        let _ = mkdir_at(&tables, None, b"/d1", 0o755, 0).map_err(|_| "mkdir d1")?;
        let _ = mkdir_at(&tables, None, b"/d2", 0o755, 0).map_err(|_| "mkdir d2")?;
        let _ = mkdir_at(&tables, None, b"/d1/sub", 0o755, 0).map_err(|_| "mkdir d1/sub")?;
        let _ = open_inode(
            &tables,
            None,
            b"/d1/sub/file.txt",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT),
            0o644,
            0,
        )
        .map_err(|_| "create d1/sub/file.txt")?;
        rename_at2(&tables, None, b"/d1/sub", None, b"/d2/sub_moved", RenameFlags::default())
            .map_err(|_| "rename cross-dir")?;
        let parent_via_dotdot = resolve_path(
            &tables,
            None,
            b"/d2/sub_moved/..",
            ResolveFlags { follow_final: true, empty_path: false, require_dir: true },
        )
        .map_err(|_| "resolve ..")?
        .inode;
        let d2_inode = resolve_path(
            &tables,
            None,
            b"/d2",
            ResolveFlags { follow_final: true, empty_path: false, require_dir: true },
        )
        .map_err(|_| "resolve d2")?
        .inode;
        kassert_eq!(parent_via_dotdot.ino(), d2_inode.ino());

        // EXCHANGE between /d2/sub_moved and a file in /d1.
        let (ex_f, _) = open_inode(
            &tables,
            None,
            b"/d1/ex.txt",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT),
            0o644,
            0,
        )
        .map_err(|_| "create ex.txt")?;
        ex_f.write_at(0, b"ex").map_err(|_| "write ex")?;
        drop(ex_f);
        rename_at2(&tables, None, b"/d2/sub_moved", None, b"/d1/ex.txt", RenameFlags(RenameFlags::EXCHANGE))
            .map_err(|_| "rename EXCHANGE")?;
        let moved_back = resolve_path(
            &tables,
            None,
            b"/d1/ex.txt/file.txt",
            ResolveFlags { follow_final: true, empty_path: false, require_dir: false },
        )
        .map_err(|_| "resolve swapped dir")?;
        kassert_eq!(moved_back.inode.inode_type(), InodeType::File);

        // Read-only mount check.
        let ro_dev = MemBlkDev::new(mem_dev.0.data.lock().clone(), true);
        let ro_bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(ro_dev, 32).map_err(|_| "ro bcache")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bcache, DevId::makedev(0, 11), true).map_err(|_| "ro mount")?;
        kassert!(ro_fs.read_only());

        // Stale unlinked inode with open_count == 0 returns NoEnt (not freed disk reuse).
        let dead_dir = mkdir_at(&tables, None, b"/dead_dir", 0o755, 0).map_err(|_| "mkdir dead_dir")?;
        unlink_at(&tables, None, b"/dead_dir", true).map_err(|_| "rmdir dead_dir")?;
        kassert!(matches!(dead_dir.create("child", 0o644), Err(VfsError::NoEnt)));
        kassert!(matches!(dead_dir.lookup("child"), Err(VfsError::NoEnt)));
        let mut dead_entries = Vec::new();
        kassert!(matches!(dead_dir.readdir(0, &mut dead_entries, 16), Err(VfsError::NoEnt)));

        // Malformed 64-bit superblock with desc_size != 64 returns Err(Inval) without panic.
        let mut bad_img = build_minimal_ext4_image_blocks(64);
        bad_img[1024 + 0xfe..1024 + 0x100].copy_from_slice(&32u16.to_le_bytes());
        let bad_dev = MemBlkDev::new(bad_img, true);
        let bad_bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(bad_dev, 16).map_err(|_| "bad bcache")?));
        kassert!(matches!(ext4::Ext4FileSystem::mount(bad_bcache, DevId::makedev(0, 12), true), Err(VfsError::Inval)));

        Ok(())
    }
);

fn kassert_ne_u64(a: u64, b: u64) -> Result<(), &'static str> {
    if a == b { Err("expected distinct u64 values") } else { Ok(()) }
}

crate::ktest!(
    fn vfs_boot_mounts_tmp_dev_proc_work() {
        use crate::{kassert, kassert_eq};
        // Boot init mounted `/`, `/tmp`, `/dev`, `/proc`.
        let mounts = list_mounts();
        kassert!(mounts.iter().any(|m| m.mount_path == "/"));
        kassert!(mounts.iter().any(|m| m.mount_path == "/tmp" && m.dev_id == DevId::TMPFS && !m.read_only));
        kassert!(mounts.iter().any(|m| m.mount_path == "/dev" && m.dev_id == DevId::DEVFS && m.read_only));
        kassert!(mounts.iter().any(|m| m.mount_path == "/proc" && m.dev_id == DevId::PROCFS && m.read_only));

        let summary = match run_dev_tmp_proc_checks() {
            Ok(summary) => summary,
            Err(err) => {
                log::error!(target: "vfs", "dev_tmp_proc_checks: {err}");
                return Err("dev_tmp_proc_checks failed (its reason is logged above)");
            }
        };
        kassert!(summary.contains("maps_ok"));
        kassert_eq!(mounts.len() >= 4, true);
        Ok(())
    }
);

crate::ktest!(
    fn vfs_proc_checks_unregister_their_process_on_failure() {
        use crate::linux::process;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering;
        let before = process::active_pids();
        FAIL_PROC_CHECKS.store(true, Ordering::Release);
        let res = run_dev_tmp_proc_checks();
        FAIL_PROC_CHECKS.store(false, Ordering::Release);
        kassert!(matches!(&res, Err(e) if e.contains("injected failure")));
        kassert_eq!(process::active_pids(), before);
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_truncate_returns_quota() {
        use crate::{kassert, kassert_eq};
        let tfs = tmpfs::Tmpfs::new(DevId::makedev(0, 121), page_cache::PAGE_SIZE);
        let f = tfs.root_inode().create("q.bin", 0o644).map_err(|_| "tmpfs create q.bin")?;
        let page = vec![0x5Au8; page_cache::PAGE_SIZE];
        kassert_eq!(f.write_at(0, &page).map_err(|_| "write page 0")?, page.len());
        kassert!(matches!(f.write_at(page.len() as u64, &page), Err(VfsError::NoSpc)));
        // Shrinking to 0 frees the page, so it can be written again.
        f.truncate(0).map_err(|_| "truncate 0")?;
        kassert_eq!(f.write_at(0, &page).map_err(|_| "write after truncate")?, page.len());
        Ok(())
    }
);

crate::ktest!(
    fn vfs_read_all_refuses_a_file_past_its_cap() {
        // `vfstest cat` and `verify-pattern` read whole files from the shell:
        // one past the cap is refused, not grown into until the heap panics.
        use crate::kassert_eq;
        let (f, _) = open_inode(
            &TABLES,
            None,
            b"/tmp/read_all_cap.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create")?;
        let res = (|| -> Result<(), &'static str> {
            f.truncate(READ_ALL_MAX as u64).map_err(|_| "truncate to the cap")?;
            kassert_eq!(read_all_inode(&f).map_err(|_| "read at the cap")?.len(), READ_ALL_MAX);
            f.truncate(READ_ALL_MAX as u64 + 1).map_err(|_| "truncate past the cap")?;
            kassert_eq!(read_all_inode(&f).err(), Some(VfsError::FBig));
            Ok(())
        })();
        unlink_at(&TABLES, None, b"/tmp/read_all_cap.bin", false).map_err(|_| "unlink")?;
        res
    }
);

crate::ktest!(
    fn vfs_sync_continues_past_a_failing_inode() {
        use crate::{kassert, kassert_eq};
        let m = mount_test_ext4(build_minimal_ext4_image_blocks(256), 130)?;
        let files = [("sync_a.bin", 0xA1u8), ("sync_b.bin", 0xB2u8)];
        let mut held = Vec::new();
        for (name, byte) in files {
            let path = format!("/{name}");
            let (f, _) = open_inode(
                &m.tables,
                None,
                path.as_bytes(),
                OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                0o644,
                0,
            )
            .map_err(|_| "create sync file")?;
            kassert_eq!(f.write_at(0, &[byte; 4096]).map_err(|_| "write sync file")?, 4096);
            held.push(f);
        }
        let cached = m.tables.snapshot_inodes();
        kassert!(held.iter().all(|f| cached.iter().any(|c| c.lock_id() == f.lock_id())));

        // The sync's first block write fails, once: the sync reports it, and
        // everything else (the other file, the filesystem) is still flushed.
        m.fault_cache.arm_write_fail_on(1);
        kassert!(matches!(sync_tables(&m.tables), Err(VfsError::Io)));
        m.fault_cache.arm_write_fail_on(0);

        let ro = mount_ro_snapshot(&m, 131)?;
        let root = ro.root_inode();
        for (name, byte) in files {
            let f = root.lookup(name).map_err(|_| "ro lookup sync file")?;
            kassert_eq!(read_all_inode(&f).map_err(|_| "ro read sync file")?, vec![byte; 4096]);
        }
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_global_reclaim_writes_back_and_evicts() {
        use crate::{kassert, kassert_eq};
        use lumen_bcache::BlockCache;

        // The ext4 page count is global: pages other ktests left cached, in
        // caches that `snapshot_cached_inodes` doesn't see, count too, so
        // every check is against the count before this test's mount. It
        // assumes `base` holds only such pages, out of reclaim's reach (the
        // ktest kernel's root is tmpfs): otherwise reclaim could evict them in
        // place of this test's pages.
        let base = page_cache::ext4_cached_pages();
        let img = build_minimal_ext4_image_blocks(256);
        let mem_dev = MemBlkDev::new(img, false);
        let bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(mem_dev, 64).map_err(|_| "bcache init")?));
        let dev = DevId::makedev(0, 99);
        let fs = ext4::Ext4FileSystem::mount(Arc::clone(&bcache), dev, false).map_err(|_| "ext4 mount")?;

        let mp = mkdir_at(&TABLES, None, b"/tmp/reclaim_mnt", 0o755, 0).map_err(|_| "mkdir mnt")?;
        let mp_dev = mp.dev_id();
        let mp_ino = mp.ino();
        if TABLES.mount(mp, "/tmp/reclaim_mnt", Arc::clone(&fs) as Arc<dyn FileSystem>).is_err() {
            if let Err(e) = unlink_at(&TABLES, None, b"/tmp/reclaim_mnt", true) {
                crate::ktest::diag(format_args!("rmdir /tmp/reclaim_mnt after the failed mount: {e:?}"));
            }
            return Err("mount");
        }
        EXT4_FILESYSTEMS.lock().push(Arc::clone(&fs));

        let result = (|| -> Result<(), &'static str> {
            let (f, _) = open_inode(
                &TABLES,
                None,
                b"/tmp/reclaim_mnt/big.bin",
                OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                0o644,
                0,
            )
            .map_err(|_| "create big.bin")?;

            // Write 6 pages (24 KiB) so 6 ext4 PageCache pages are dirty.
            let mut buf = vec![0u8; 6 * 4096];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((i * 73) ^ 0x19) as u8;
            }
            kassert_eq!(f.write_at(0, &buf).map_err(|_| "write big")?, buf.len());
            kassert!(page_cache::ext4_cached_pages() >= base + 6);

            // Reclaim down to 1 page past the baseline: dirty pages must be written back before eviction.
            page_cache::reclaim_ext4_to_target(base + 1);
            kassert!(page_cache::ext4_cached_pages() <= base + 1);

            // Read back after eviction (re-faults from disk) and verify contents match.
            let got = read_all_inode(&f).map_err(|_| "read after reclaim")?;
            kassert_eq!(got, buf);
            drop(f);

            // Dirty pages reclaimed without a sync, then unlinked while open: the
            // detached inode reads back what was written (write_at -> reclaim to the baseline ->
            // OpenGuard + unlink_at -> read_at).
            let (victim, _) = open_inode(
                &TABLES,
                None,
                b"/tmp/reclaim_mnt/victim.bin",
                OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                0o644,
                0,
            )
            .map_err(|_| "create victim.bin")?;
            let victim_payload = vec![0x5Au8; 2 * 4096];
            kassert_eq!(victim.write_at(0, &victim_payload).map_err(|_| "write victim")?, victim_payload.len());
            page_cache::reclaim_ext4_to_target(base);
            kassert!(page_cache::ext4_cached_pages() <= base);

            let guard = OpenGuard::new_unchecked(Arc::clone(&victim));
            unlink_at(&TABLES, None, b"/tmp/reclaim_mnt/victim.bin", false).map_err(|_| "unlink victim")?;
            kassert!(victim.is_detached());
            let got_victim = read_all_inode(&victim).map_err(|_| "read detached victim")?;
            kassert_eq!(got_victim, victim_payload);
            drop(guard);
            drop(victim);
            Ok(())
        })();
        let umounted = TABLES.umount(mp_dev, mp_ino).map(drop).map_err(|_| "umount");
        EXT4_FILESYSTEMS.lock().retain(|e| e.dev_id() != dev);
        let unlinked = unlink_at(&TABLES, None, b"/tmp/reclaim_mnt", true).map(drop).map_err(|_| "rmdir mnt");
        let cleanup = umounted.and(unlinked);
        if let (Err(_), Err(e)) = (&result, &cleanup) {
            // The body's error is returned; a mount left behind could still
            // break later ktests, so say so.
            crate::ktest::diag(format_args!("cleanup after the failure failed too: {e}"));
        }
        result.and(cleanup)
    }
);

crate::ktest!(
    fn vfs_tmpfs_quota_rmdir_maps_heap_and_procfs_parsing() {
        use crate::{kassert, kassert_eq};

        // 1. tmpfs multi-page -ENOSPC short-write + truncate extend does not leak unwritten data.
        let tfs = tmpfs::Tmpfs::new(DevId::makedev(0, 120), page_cache::PAGE_SIZE);
        let root = tfs.root_inode();
        let f = root.create("short.bin", 0o644).map_err(|_| "tmpfs create short.bin")?;
        let two_pages = vec![0xA5u8; 2 * page_cache::PAGE_SIZE];
        kassert_eq!(f.write_at(0, &two_pages).map_err(|_| "short write")?, page_cache::PAGE_SIZE);
        kassert_eq!(f.stat().map_err(|_| "stat short")?.size, page_cache::PAGE_SIZE as u64);
        kassert!(matches!(f.write_at(page_cache::PAGE_SIZE as u64, &[0xBBu8; 64]), Err(VfsError::NoSpc)));

        // Free quota by truncating to 0, then extend past 2 pages: must read all zeroes.
        f.truncate(0).map_err(|_| "truncate 0")?;
        f.truncate((2 * page_cache::PAGE_SIZE) as u64).map_err(|_| "truncate extend")?;
        let extended = read_all_inode(&f).map_err(|_| "read extended")?;
        kassert_eq!(extended.len(), 2 * page_cache::PAGE_SIZE);
        kassert!(extended.iter().all(|&b| b == 0));

        // 2. tmpfs rmdir (open_count == 0) blocks subsequent create/mkdir/symlink with NoEnt.
        let d = root.mkdir("gone_dir", 0o755).map_err(|_| "mkdir gone_dir")?;
        kassert_eq!(d.open_count(), 0);
        root.rmdir("gone_dir").map_err(|_| "rmdir gone_dir")?;
        kassert!(matches!(d.create("f", 0o644), Err(VfsError::NoEnt)));
        kassert!(matches!(d.mkdir("sub", 0o755), Err(VfsError::NoEnt)));
        kassert!(matches!(d.symlink("sym", "/x"), Err(VfsError::NoEnt)));
        kassert!(matches!(d.lookup("f"), Err(VfsError::NoEnt)));

        // 3. AddressSpace::format_maps coalesced .bss + brk emits [heap] at brk_init.
        let elf_img = crate::mm::user_vm::build_relro_test_elf();
        let (space, prog) = crate::mm::user_vm::AddressSpace::from_elf(&elf_img, &[b"/t"], &[], b"/t", [0; 16])
            .map_err(|_| "from_elf")?;
        kassert_eq!(prog.initial_brk, 0x40_4000);
        kassert_eq!(space.brk(0x40_5000), 0x40_5000);
        let maps = space.format_maps();
        kassert!(maps.contains("00402000-00404000 rw-p 00000000 00:00 0\n"));
        // `[heap]` after 73 characters, where Linux pads a name to.
        let heap = super::ktest_maps_line("00404000-00405000 rw-p 00000000 00:00 0", "[heap]");
        kassert!(maps.contains(&heap));
        // Every line has the strict Linux shape, the text segment is r-xp,
        // and the stack is labelled.
        kassert!(maps.lines().all(is_maps_line));
        kassert!(maps.lines().any(|l| l.split(' ').nth(1) == Some("r-xp")));
        kassert!(maps.lines().any(|l| l.ends_with("  [stack]")));
        kassert!(!is_maps_line("400000-401000 r-xp 00000000 00:00 0"));
        kassert!(!is_maps_line("00400000-00401000 r-xp 0 00:00 0"));
        kassert!(!is_maps_line("00400000-00401000 rwxq 00000000 00:00 0"));
        kassert!(!is_maps_line("00400000-00401000 r-xp 00000000 00:00 0 "));
        kassert!(!is_maps_line("00404000-00405000 rw-p 00000000 00:00 0  [heap]"));
        kassert!(is_maps_line(heap.trim_end()));
        kassert!(is_maps_line(super::ktest_maps_line("00400000-00401000 r-xp 00000000 00:02 17", "/bin/x").trim_end()));
        space.teardown();

        // 4. procfs: numeric names must be canonical. (That pids above 2^16
        // get distinct inode numbers is host-tested in `lumen_vfs::procfs`.)
        let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };
        kassert!(matches!(resolve_path(&TABLES, None, b"/proc/+1", rf), Err(VfsError::NoEnt)));
        kassert!(matches!(resolve_path(&TABLES, None, b"/proc/01", rf), Err(VfsError::NoEnt)));
        Ok(())
    }
);

struct FaultInjectBlockCache {
    inner: SleepMutex<lumen_bcache::BlockCache<MemBlkDev>>,
    fail_reads: core::sync::atomic::AtomicBool,
    fail_writes: core::sync::atomic::AtomicBool,
    fail_write_on_call: core::sync::atomic::AtomicUsize,
    fail_write_on_call2: core::sync::atomic::AtomicUsize,
    fail_write_from_call: core::sync::atomic::AtomicUsize,
    write_calls: core::sync::atomic::AtomicUsize,
}

impl FaultInjectBlockCache {
    fn new(inner: lumen_bcache::BlockCache<MemBlkDev>) -> Self {
        Self {
            inner: SleepMutex::new(inner),
            fail_reads: core::sync::atomic::AtomicBool::new(false),
            fail_writes: core::sync::atomic::AtomicBool::new(false),
            fail_write_on_call: core::sync::atomic::AtomicUsize::new(0),
            fail_write_on_call2: core::sync::atomic::AtomicUsize::new(0),
            fail_write_from_call: core::sync::atomic::AtomicUsize::new(0),
            write_calls: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn arm_write_fail_on(&self, call_num: usize) {
        self.write_calls.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call.store(call_num, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call2.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_from_call.store(0, core::sync::atomic::Ordering::Release);
    }

    fn arm_write_fail_pair(&self, call_1: usize, call_2: usize) {
        self.write_calls.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call.store(call_1, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call2.store(call_2, core::sync::atomic::Ordering::Release);
        self.fail_write_from_call.store(0, core::sync::atomic::Ordering::Release);
    }

    fn arm_write_fail_from(&self, from_call: usize) {
        self.write_calls.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_on_call2.store(0, core::sync::atomic::Ordering::Release);
        self.fail_write_from_call.store(from_call, core::sync::atomic::Ordering::Release);
    }
}

impl ext4::BlockCacheBackend for FaultInjectBlockCache {
    fn read_at(&self, byte_off: u64, dst: &mut [u8]) -> Result<(), lumen_bcache::BlockError> {
        if self.fail_reads.load(core::sync::atomic::Ordering::Acquire) {
            return Err(lumen_bcache::BlockError::Io);
        }
        self.inner.lock().read_at(byte_off, dst)
    }

    fn write_at(&self, byte_off: u64, src: &[u8]) -> Result<(), lumen_bcache::BlockError> {
        if self.fail_writes.load(core::sync::atomic::Ordering::Acquire) {
            return Err(lumen_bcache::BlockError::Io);
        }
        let call_idx = self.write_calls.fetch_add(1, core::sync::atomic::Ordering::AcqRel) + 1;
        let target = self.fail_write_on_call.load(core::sync::atomic::Ordering::Acquire);
        let target2 = self.fail_write_on_call2.load(core::sync::atomic::Ordering::Acquire);
        let from_call = self.fail_write_from_call.load(core::sync::atomic::Ordering::Acquire);
        if (target != 0 && call_idx == target)
            || (target2 != 0 && call_idx == target2)
            || (from_call != 0 && call_idx >= from_call)
        {
            return Err(lumen_bcache::BlockError::Io);
        }
        self.inner.lock().write_at(byte_off, src)
    }

    fn sync(&self) -> Result<(), lumen_bcache::BlockError> {
        if self.fail_writes.load(core::sync::atomic::Ordering::Acquire) {
            return Err(lumen_bcache::BlockError::Io);
        }
        self.inner.lock().sync()
    }
}

crate::ktest!(
    fn ext4_flush_and_reclaim_preserve_dirty_on_io_error() {
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering;
        use lumen_bcache::BlockCache;

        let img = build_minimal_ext4_image_blocks(256);
        let mem_dev = MemBlkDev::new(img, false);
        let bcache_raw = BlockCache::new(mem_dev.clone(), 64).map_err(|_| "bcache init")?;
        let fault_cache = Arc::new(FaultInjectBlockCache::new(bcache_raw));
        let bcache: Arc<dyn ext4::BlockCacheBackend> = Arc::clone(&fault_cache) as Arc<dyn ext4::BlockCacheBackend>;
        let fs = ext4::Ext4FileSystem::mount(bcache, DevId::makedev(0, 13), false).map_err(|_| "ext4 mount")?;

        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;

        // 1. Create a file and write 2 dirty pages in PageCache without syncing yet.
        let (f, _) = open_inode(
            &tables,
            None,
            b"/dirty_err.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create dirty_err.bin")?;
        let mut payload = vec![0u8; 2 * 4096];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = ((i * 37) ^ 0xA5) as u8;
        }
        kassert_eq!(f.write_at(0, &payload).map_err(|_| "write dirty_err")?, payload.len());
        let ext4_inode = fs.lookup_live_by_lock_id(f.lock_id()).ok_or("lookup live inode")?;

        // 2. Fail Inode::read during flush_to_ext4 (sync) and reclaim_dirty_pages:
        //    both must preserve page dirty state so clean-page reclamation cannot drop
        //    modified pages and a retry sync writes them to disk.
        fault_cache.fail_reads.store(true, Ordering::Release);
        kassert!(matches!(f.sync(false), Err(VfsError::Io)));
        kassert_eq!(ext4_inode.reclaim_clean_pages(16), 0);
        kassert_eq!(ext4_inode.reclaim_dirty_pages(16), 0);
        kassert_eq!(ext4_inode.reclaim_clean_pages(16), 0);

        // 3. Recover I/O: retry sync must succeed and persist both modified pages.
        fault_cache.fail_reads.store(false, Ordering::Release);
        f.sync(false).map_err(|_| "retry sync after read error")?;
        kassert_eq!(ext4_inode.reclaim_clean_pages(16), 2);
        kassert_eq!(read_all_inode(&f).map_err(|_| "re-fault dirty_err")?, payload);
        drop(ext4_inode);
        drop(f);

        // 4. Also verify write-side failure during writeback preserves dirty state.
        let (w_f, _) = open_inode(
            &tables,
            None,
            b"/write_err.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create write_err.bin")?;
        let old_data = vec![0x11u8; 4096];
        let new_data = vec![0x22u8; 4096];
        kassert_eq!(w_f.write_at(0, &old_data).map_err(|_| "write old_data")?, old_data.len());
        w_f.sync(false).map_err(|_| "initial sync write_err")?;
        kassert_eq!(w_f.write_at(0, &new_data).map_err(|_| "write new_data")?, new_data.len());

        let w_ext4 = fs.lookup_live_by_lock_id(w_f.lock_id()).ok_or("lookup live write_err")?;
        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(w_f.sync(false), Err(VfsError::Io)));
        kassert_eq!(w_ext4.reclaim_clean_pages(16), 0);
        fault_cache.fail_writes.store(false, Ordering::Release);
        w_f.sync(false).map_err(|_| "retry sync after write error")?;
        kassert_eq!(w_ext4.reclaim_clean_pages(16), 1);
        kassert_eq!(read_all_inode(&w_f).map_err(|_| "re-fault write_err")?, new_data);
        drop(w_ext4);
        drop(w_f);

        // 5. Verify persisted bytes on a fresh read-only mount of the backing image.
        let ro_dev = MemBlkDev::new(mem_dev.0.data.lock().clone(), true);
        let ro_bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(ro_dev, 32).map_err(|_| "ro bcache")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bcache, DevId::makedev(0, 14), true).map_err(|_| "ro mount")?;
        let ro_root = ro_fs.root_inode();
        let ro_dirty = ro_root.lookup("dirty_err.bin").map_err(|_| "ro lookup dirty_err")?;
        kassert_eq!(read_all_inode(&ro_dirty).map_err(|_| "ro read dirty_err")?, payload);
        let ro_write = ro_root.lookup("write_err.bin").map_err(|_| "ro lookup write_err")?;
        kassert_eq!(read_all_inode(&ro_write).map_err(|_| "ro read write_err")?, new_data);

        Ok(())
    }
);

crate::ktest!(
    fn ext4_truncate_and_open_unlink_preserve_state_on_io_error() {
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering;
        use lumen_bcache::BlockCache;

        let img = build_minimal_ext4_image_blocks(256);
        let mem_dev = MemBlkDev::new(img, false);
        let bcache_raw = BlockCache::new(mem_dev.clone(), 64).map_err(|_| "bcache init")?;
        let fault_cache = Arc::new(FaultInjectBlockCache::new(bcache_raw));
        let bcache: Arc<dyn ext4::BlockCacheBackend> = Arc::clone(&fault_cache) as Arc<dyn ext4::BlockCacheBackend>;
        let fs = ext4::Ext4FileSystem::mount(bcache, DevId::makedev(0, 15), false).map_err(|_| "ext4 mount")?;

        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;

        // 1. truncate shrink under fail_reads and fail_writes preserves meta.size and dirty PageCache bytes.
        let (tf, _) = open_inode(
            &tables,
            None,
            b"/trunc_err.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create trunc_err.bin")?;
        let mut payload_1 = vec![0u8; 2 * 4096];
        let mut payload_2 = vec![0u8; 2 * 4096];
        for i in 0..payload_1.len() {
            payload_1[i] = ((i * 19) ^ 0x5A) as u8;
            payload_2[i] = ((i * 41) ^ 0xC3) as u8;
        }
        kassert_eq!(tf.write_at(0, &payload_1).map_err(|_| "write payload_1")?, payload_1.len());

        fault_cache.fail_reads.store(true, Ordering::Release);
        kassert!(matches!(tf.truncate(100), Err(VfsError::Io)));
        kassert_eq!(tf.stat().map_err(|_| "stat after read err")?.size, 8192);
        kassert_eq!(read_all_inode(&tf).map_err(|_| "read after trunc read err")?, payload_1);
        fault_cache.fail_reads.store(false, Ordering::Release);

        tf.sync(false).map_err(|_| "sync payload_1")?;
        kassert_eq!(tf.write_at(0, &payload_2).map_err(|_| "write payload_2")?, payload_2.len());
        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(tf.truncate(100), Err(VfsError::Io)));
        kassert_eq!(tf.stat().map_err(|_| "stat after write err")?.size, 8192);
        kassert_eq!(read_all_inode(&tf).map_err(|_| "read after trunc write err")?, payload_2);
        fault_cache.fail_writes.store(false, Ordering::Release);

        tf.truncate(100).map_err(|_| "retry truncate shrink")?;
        kassert_eq!(tf.stat().map_err(|_| "stat after shrink")?.size, 100);
        kassert_eq!(read_all_inode(&tf).map_err(|_| "read after shrink")?, payload_2[..100]);

        fault_cache.fail_reads.store(true, Ordering::Release);
        kassert!(matches!(tf.truncate(200), Err(VfsError::Io)));
        kassert_eq!(tf.stat().map_err(|_| "stat after extend err")?.size, 100);
        fault_cache.fail_reads.store(false, Ordering::Release);

        tf.truncate(200).map_err(|_| "retry truncate extend")?;
        let extended = read_all_inode(&tf).map_err(|_| "read after extend")?;
        kassert_eq!(extended.len(), 200);
        kassert_eq!(&extended[..100], &payload_2[..100]);
        kassert!(extended[100..200].iter().all(|&b| b == 0));
        tf.truncate(100).map_err(|_| "final shrink 100")?;
        tf.sync(false).map_err(|_| "sync trunc_err")?;
        drop(tf);

        // 2. unlink on open file (open_count > 0, nlink == 1) under disk I/O error preserves
        //    nlink == 1, live_inodes/path lookup, meta_dirty/dirty contents, and DETACHED_QUOTA.
        let (uf, _) = open_inode(
            &tables,
            None,
            b"/open_unlink_err.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create open_unlink_err.bin")?;
        kassert_eq!(uf.write_at(0, &payload_1).map_err(|_| "write u_payload")?, payload_1.len());
        uf.sync(false).map_err(|_| "sync u_payload")?;
        let u_ext4 = fs.lookup_live_by_lock_id(uf.lock_id()).ok_or("lookup live u_ext4")?;
        kassert_eq!(u_ext4.reclaim_clean_pages(16), 2);
        kassert_eq!(uf.write_at(8192, b"EXTRA").map_err(|_| "write extra tail")?, 5);

        let ug = OpenGuard::new_unchecked(Arc::clone(&uf));
        let orig_ino = uf.ino();
        let quota_before = page_cache::DETACHED_QUOTA.used_pages();
        let detached_before = page_cache::detached_cached_pages();
        let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(unlink_at(&tables, None, b"/open_unlink_err.bin", false), Err(VfsError::Io)));
        kassert!(!uf.is_detached());
        kassert_eq!(uf.ino(), orig_ino);
        kassert_eq!(uf.stat().map_err(|_| "stat after unlink err")?.nlink, 1);
        kassert!(fs.lookup_live_by_lock_id(uf.lock_id()).is_some());
        kassert_eq!(
            resolve_path(&tables, None, b"/open_unlink_err.bin", rf).map_err(|_| "still linked")?.inode.ino(),
            orig_ino
        );
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        kassert_eq!(page_cache::detached_cached_pages(), detached_before);
        kassert_eq!(u_ext4.reclaim_clean_pages(16), 0);
        let mut expected_u = payload_1.clone();
        expected_u.extend_from_slice(b"EXTRA");
        kassert_eq!(read_all_inode(&uf).map_err(|_| "read after failed unlink")?, expected_u);
        fault_cache.fail_writes.store(false, Ordering::Release);

        // meta_dirty must still be true so fdatasync (sync(true)) writes page 2 and updates size.
        uf.sync(true).map_err(|_| "sync(true) preserves meta_dirty")?;
        unlink_at(&tables, None, b"/open_unlink_err.bin", false).map_err(|_| "retry open unlink")?;
        kassert!(uf.is_detached());
        kassert_ne_u64(uf.ino(), orig_ino)?;
        kassert_eq!(uf.stat().map_err(|_| "stat detached")?.nlink, 0);
        kassert!(fs.lookup_live_by_lock_id(uf.lock_id()).is_none());
        kassert!(matches!(resolve_path(&tables, None, b"/open_unlink_err.bin", rf), Err(VfsError::NoEnt)));
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before + 3);
        kassert_eq!(read_all_inode(&uf).map_err(|_| "read detached after unlink")?, expected_u);
        drop(u_ext4);
        drop(ug);
        drop(uf);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        kassert_eq!(page_cache::detached_cached_pages(), detached_before);

        // 3. rmdir and rename overwrite on open targets under fail_writes preserve live state.
        let dir_ino = mkdir_at(&tables, None, b"/open_rmdir", 0o755, 0).map_err(|_| "mkdir open_rmdir")?;
        let dg = OpenGuard::new_unchecked(Arc::clone(&dir_ino));
        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(unlink_at(&tables, None, b"/open_rmdir", true), Err(VfsError::Io)));
        kassert!(!dir_ino.is_detached());
        kassert_eq!(dir_ino.stat().map_err(|_| "stat open_rmdir")?.nlink, 2);
        kassert!(fs.lookup_live_by_lock_id(dir_ino.lock_id()).is_some());
        fault_cache.fail_writes.store(false, Ordering::Release);
        unlink_at(&tables, None, b"/open_rmdir", true).map_err(|_| "retry open rmdir")?;
        kassert!(dir_ino.is_detached());
        kassert_eq!(dir_ino.stat().map_err(|_| "stat detached rmdir")?.nlink, 0);
        drop(dg);
        drop(dir_ino);

        let (rsrc, _) = open_inode(
            &tables,
            None,
            b"/ren_src.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create ren_src")?;
        kassert_eq!(rsrc.write_at(0, b"NEW").map_err(|_| "write ren_src")?, 3);
        drop(rsrc);
        let (rdst, _) = open_inode(
            &tables,
            None,
            b"/ren_dst.bin",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create ren_dst")?;
        kassert_eq!(rdst.write_at(0, b"OLD").map_err(|_| "write ren_dst")?, 3);
        let rg = OpenGuard::new_unchecked(Arc::clone(&rdst));
        fault_cache.fail_writes.store(true, Ordering::Release);
        kassert!(matches!(
            rename_at2(&tables, None, b"/ren_src.bin", None, b"/ren_dst.bin", RenameFlags::default()),
            Err(VfsError::Io)
        ));
        kassert!(!rdst.is_detached());
        kassert_eq!(rdst.stat().map_err(|_| "stat ren_dst")?.nlink, 1);
        kassert!(fs.lookup_live_by_lock_id(rdst.lock_id()).is_some());
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        fault_cache.fail_writes.store(false, Ordering::Release);
        rename_at2(&tables, None, b"/ren_src.bin", None, b"/ren_dst.bin", RenameFlags::default())
            .map_err(|_| "retry rename overwrite")?;
        kassert!(rdst.is_detached());
        kassert_eq!(rdst.stat().map_err(|_| "stat detached ren_dst")?.nlink, 0);
        kassert_eq!(read_all_inode(&rdst).map_err(|_| "read detached ren_dst")?, b"OLD");
        drop(rg);
        drop(rdst);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);

        // 4. Verify persisted truncated prefix and renamed file on fresh RO mount.
        fs.sync().map_err(|_| "final fs.sync")?;
        let ro_dev = MemBlkDev::new(mem_dev.0.data.lock().clone(), true);
        let ro_bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(ro_dev, 32).map_err(|_| "ro bcache")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bcache, DevId::makedev(0, 16), true).map_err(|_| "ro mount")?;
        let ro_root = ro_fs.root_inode();
        let ro_trunc = ro_root.lookup("trunc_err.bin").map_err(|_| "ro lookup trunc_err")?;
        kassert_eq!(read_all_inode(&ro_trunc).map_err(|_| "ro read trunc_err")?, payload_2[..100]);
        let ro_ren = ro_root.lookup("ren_dst.bin").map_err(|_| "ro lookup ren_dst")?;
        kassert_eq!(read_all_inode(&ro_ren).map_err(|_| "ro read ren_dst")?, b"NEW");

        Ok(())
    }
);

struct TestExt4Mount {
    mem_dev: MemBlkDev,
    fault_cache: Arc<FaultInjectBlockCache>,
    fs: Arc<ext4::Ext4FileSystem>,
    tables: KernelVfsTables,
}

fn mount_test_ext4(img: Vec<u8>, dev_minor: u32) -> Result<TestExt4Mount, &'static str> {
    use lumen_bcache::BlockCache;
    let mem_dev = MemBlkDev::new(img, false);
    let bcache_raw = BlockCache::new(mem_dev.clone(), 32).map_err(|_| "bcache init")?;
    let fault_cache = Arc::new(FaultInjectBlockCache::new(bcache_raw));
    let bcache: Arc<dyn ext4::BlockCacheBackend> = Arc::clone(&fault_cache) as Arc<dyn ext4::BlockCacheBackend>;
    let fs = ext4::Ext4FileSystem::mount(bcache, DevId::makedev(0, dev_minor), false).map_err(|_| "ext4 mount")?;
    let tables = KernelVfsTables::new();
    tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;
    Ok(TestExt4Mount { mem_dev, fault_cache, fs, tables })
}

fn mount_ro_snapshot(m: &TestExt4Mount, dev_minor: u32) -> Result<Arc<ext4::Ext4FileSystem>, &'static str> {
    use ext4::BlockCacheBackend;
    use lumen_bcache::BlockCache;
    m.fault_cache.sync().map_err(|_| "fault_cache sync")?;
    let ro_dev = MemBlkDev::new(m.mem_dev.0.data.lock().clone(), true);
    let ro_bcache: Arc<dyn ext4::BlockCacheBackend> =
        Arc::new(SleepMutex::new(BlockCache::new(ro_dev, 32).map_err(|_| "ro bcache")?));
    ext4::Ext4FileSystem::mount(ro_bcache, DevId::makedev(0, dev_minor), true).map_err(|_| "ro mount")
}

#[inline(never)]
fn sweep_rmdir_io_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    let m = mount_test_ext4(base_img.to_vec(), 20)?;
    let d = mkdir_at(&m.tables, None, b"/victim_dir", 0o755, 0).map_err(|_| "mkdir victim_dir")?;
    drop(d);
    m.fs.sync().map_err(|_| "sync victim_dir")?;
    let snap = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    unlink_at(&m.tables, None, b"/victim_dir", true).map_err(|_| "measure rmdir")?;
    let total_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(total_writes >= 10);
    drop(m);

    for &open in &[true, false] {
        for fail_at in 1..=total_writes {
            let m = mount_test_ext4(snap.clone(), 21)?;
            let d = resolve_path(&m.tables, None, b"/victim_dir", rf).map_err(|_| "resolve victim_dir")?.inode;
            let orig_ino = d.ino();
            let guard = if open { Some(OpenGuard::new_unchecked(Arc::clone(&d))) } else { None };
            m.fault_cache.arm_write_fail_on(fail_at);
            let res = unlink_at(&m.tables, None, b"/victim_dir", true);
            m.fault_cache.arm_write_fail_on(0);

            match res {
                Ok(()) => {
                    kassert!(matches!(resolve_path(&m.tables, None, b"/victim_dir", rf), Err(VfsError::NoEnt)));
                    if open {
                        kassert!(d.is_detached());
                        kassert_ne_u64(d.ino(), orig_ino)?;
                    }
                    kassert_eq!(d.stat().map_err(|_| "stat ok")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(d.lock_id()).is_none());
                    let (nf, _) = open_inode(
                        &m.tables,
                        None,
                        b"/new_after_rmdir",
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create after ok rmdir")?;
                    kassert_eq!(nf.inode_type(), InodeType::File);
                }
                Err(VfsError::Io) => {
                    if resolve_path(&m.tables, None, b"/victim_dir", rf).is_ok() {
                        kassert!(!d.is_detached());
                        kassert_eq!(d.stat().map_err(|_| "stat still live")?.nlink, 2);
                        kassert!(m.fs.lookup_live_by_lock_id(d.lock_id()).is_some());
                        unlink_at(&m.tables, None, b"/victim_dir", true).map_err(|_| "retry rmdir")?;
                        if open {
                            kassert!(d.is_detached());
                        }
                        kassert_eq!(d.stat().map_err(|_| "stat retried")?.nlink, 0);
                    } else {
                        if open {
                            kassert!(d.is_detached());
                            kassert_ne_u64(d.ino(), orig_ino)?;
                        }
                        kassert_eq!(d.stat().map_err(|_| "stat removed")?.nlink, 0);
                        kassert!(m.fs.lookup_live_by_lock_id(d.lock_id()).is_none());
                        let (nf, _) = open_inode(
                            &m.tables,
                            None,
                            b"/new_after_rmdir",
                            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                            0o644,
                            0,
                        )
                        .map_err(|_| "create after partial rmdir")?;
                        kassert_eq!(nf.inode_type(), InodeType::File);
                    }
                }
                Err(_) => return Err("unexpected error in rmdir sweep"),
            }
            drop(guard);
            drop(d);
            drop(m);
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        }
    }
    Ok(())
}

#[inline(never)]
fn sweep_rename_dir_overwrite_io_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    let m = mount_test_ext4(base_img.to_vec(), 22)?;
    let s = mkdir_at(&m.tables, None, b"/src_d", 0o755, 0).map_err(|_| "mkdir src_d")?;
    let d = mkdir_at(&m.tables, None, b"/dst_d", 0o755, 0).map_err(|_| "mkdir dst_d")?;
    drop((s, d));
    m.fs.sync().map_err(|_| "sync rename dir")?;
    let snap = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/src_d", None, b"/dst_d", RenameFlags::default()).map_err(|_| "measure ren dir")?;
    let total_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(total_writes >= 12);
    drop(m);

    for fail_at in 1..=total_writes {
        let m = mount_test_ext4(snap.clone(), 23)?;
        let dst = resolve_path(&m.tables, None, b"/dst_d", rf).map_err(|_| "resolve dst_d")?.inode;
        let dst_orig_ino = dst.ino();
        let src_ino = resolve_path(&m.tables, None, b"/src_d", rf).map_err(|_| "resolve src_d")?.inode.ino();
        let guard = OpenGuard::new_unchecked(Arc::clone(&dst));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/src_d", None, b"/dst_d", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);

        match res {
            Ok(()) => {
                kassert!(dst.is_detached());
                kassert_eq!(dst.stat().map_err(|_| "stat dst ok")?.nlink, 0);
                kassert!(m.fs.lookup_live_by_lock_id(dst.lock_id()).is_none());
            }
            Err(VfsError::Io) => {
                let dst_cur = resolve_path(&m.tables, None, b"/dst_d", rf);
                if matches!(dst_cur, Ok(ref r) if r.inode.ino() == dst_orig_ino) {
                    kassert!(!dst.is_detached());
                    kassert_eq!(dst.stat().map_err(|_| "stat dst live")?.nlink, 2);
                    kassert!(m.fs.lookup_live_by_lock_id(dst.lock_id()).is_some());
                    rename_at2(&m.tables, None, b"/src_d", None, b"/dst_d", RenameFlags::default())
                        .map_err(|_| "retry ren dir")?;
                    kassert!(dst.is_detached());
                } else {
                    // The error came after the source landed: `/dst_d` is
                    // the source now, never missing.
                    let moved = resolve_path(&m.tables, None, b"/dst_d", rf).map_err(|_| "dst_d lost")?.inode;
                    kassert_eq!(moved.ino(), src_ino);
                    kassert!(matches!(resolve_path(&m.tables, None, b"/src_d", rf), Err(VfsError::NoEnt)));
                    kassert!(dst.is_detached());
                    kassert_ne_u64(dst.ino(), dst_orig_ino)?;
                    kassert_eq!(dst.stat().map_err(|_| "stat dst detached")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(dst.lock_id()).is_none());
                    let (nf, _) = open_inode(
                        &m.tables,
                        None,
                        b"/new_after_ren_dir",
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create after partial ren dir")?;
                    kassert_eq!(nf.inode_type(), InodeType::File);
                }
            }
            Err(_) => return Err("unexpected error in rename dir overwrite sweep"),
        }
        drop(guard);
        drop(dst);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }
    Ok(())
}

#[inline(never)]
fn sweep_inline_symlink_and_file_unlink_io_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    // (1) Inline symlink unlink (open & closed) + rename overwrite
    let m = mount_test_ext4(base_img.to_vec(), 24)?;
    let s = symlink_at(&m.tables, b"/real/target", None, b"/sym_i").map_err(|_| "symlink sym_i")?;
    let s_src = symlink_at(&m.tables, b"/other/target", None, b"/sym_s").map_err(|_| "symlink sym_s")?;
    drop((s, s_src));
    m.fs.sync().map_err(|_| "sync syms")?;
    let snap_sym = m.mem_dev.0.data.lock().clone();

    m.fault_cache.arm_write_fail_on(0);
    unlink_at(&m.tables, None, b"/sym_i", false).map_err(|_| "measure sym unlink")?;
    let sym_unlink_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(sym_unlink_writes >= 8);
    drop(m);

    for &open in &[true, false] {
        for fail_at in 1..=sym_unlink_writes {
            let m = mount_test_ext4(snap_sym.clone(), 25)?;
            let sym = resolve_path(&m.tables, None, b"/sym_i", rf).map_err(|_| "resolve sym_i")?.inode;
            let guard = if open { Some(OpenGuard::new_unchecked(Arc::clone(&sym))) } else { None };
            m.fault_cache.arm_write_fail_on(fail_at);
            let res = unlink_at(&m.tables, None, b"/sym_i", false);
            m.fault_cache.arm_write_fail_on(0);

            match res {
                Ok(()) => {
                    kassert!(matches!(resolve_path(&m.tables, None, b"/sym_i", rf), Err(VfsError::NoEnt)));
                    if open {
                        kassert!(sym.is_detached());
                        kassert_eq!(sym.readlink().map_err(|_| "sym readlink ok")?, b"/real/target");
                    }
                    kassert!(m.fs.lookup_live_by_lock_id(sym.lock_id()).is_none());
                }
                Err(VfsError::Io) => {
                    if resolve_path(&m.tables, None, b"/sym_i", rf).is_ok() {
                        kassert!(!sym.is_detached());
                        kassert_eq!(sym.readlink().map_err(|_| "sym readlink live")?, b"/real/target");
                        kassert_eq!(
                            readlink_at(&m.tables, None, b"/sym_i").map_err(|_| "path readlink")?,
                            b"/real/target"
                        );
                        unlink_at(&m.tables, None, b"/sym_i", false).map_err(|_| "retry sym unlink")?;
                        if open {
                            kassert!(sym.is_detached());
                            kassert_eq!(sym.readlink().map_err(|_| "sym readlink detached")?, b"/real/target");
                        }
                    } else {
                        if open {
                            kassert!(sym.is_detached());
                            kassert_eq!(sym.readlink().map_err(|_| "sym readlink detached2")?, b"/real/target");
                        }
                        kassert_eq!(sym.stat().map_err(|_| "sym stat removed")?.nlink, 0);
                        kassert!(m.fs.lookup_live_by_lock_id(sym.lock_id()).is_none());
                        let nd = mkdir_at(&m.tables, None, b"/new_dir_sym", 0o755, 0).map_err(|_| "mkdir after sym")?;
                        kassert_eq!(nd.inode_type(), InodeType::Dir);
                    }
                }
                Err(_) => return Err("unexpected error in sym unlink sweep"),
            }
            drop(guard);
            drop(sym);
            drop(m);
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        }
    }

    // Inline symlink rename overwrite
    let m_ren = mount_test_ext4(snap_sym.clone(), 26)?;
    m_ren.fault_cache.arm_write_fail_on(0);
    rename_at2(&m_ren.tables, None, b"/sym_s", None, b"/sym_i", RenameFlags::default())
        .map_err(|_| "measure sym rename")?;
    let sym_ren_writes = m_ren.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(sym_ren_writes >= sym_unlink_writes);
    drop(m_ren);

    for fail_at in 1..=sym_ren_writes {
        let m = mount_test_ext4(snap_sym.clone(), 27)?;
        let sym = resolve_path(&m.tables, None, b"/sym_i", rf).map_err(|_| "resolve sym_i ren")?.inode;
        let sym_orig_ino = sym.ino();
        let src_ino = resolve_path(&m.tables, None, b"/sym_s", rf).map_err(|_| "resolve sym_s ren")?.inode.ino();
        let guard = OpenGuard::new_unchecked(Arc::clone(&sym));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sym_s", None, b"/sym_i", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);

        match res {
            Ok(()) => {
                kassert!(sym.is_detached());
                kassert_eq!(sym.readlink().map_err(|_| "sym ren ok readlink")?, b"/real/target");
                kassert!(m.fs.lookup_live_by_lock_id(sym.lock_id()).is_none());
            }
            Err(VfsError::Io) => {
                let cur = resolve_path(&m.tables, None, b"/sym_i", rf);
                if matches!(cur, Ok(ref r) if r.inode.ino() == sym_orig_ino) {
                    kassert!(!sym.is_detached());
                    kassert_eq!(sym.readlink().map_err(|_| "sym ren live readlink")?, b"/real/target");
                    rename_at2(&m.tables, None, b"/sym_s", None, b"/sym_i", RenameFlags::default())
                        .map_err(|_| "retry sym rename")?;
                    kassert!(sym.is_detached());
                } else {
                    // The error came after the source landed.
                    let moved = resolve_path(&m.tables, None, b"/sym_i", rf).map_err(|_| "sym_i lost")?.inode;
                    kassert_eq!(moved.ino(), src_ino);
                    kassert!(sym.is_detached());
                    kassert_eq!(sym.stat().map_err(|_| "sym ren detached")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(sym.lock_id()).is_none());
                    let nd =
                        mkdir_at(&m.tables, None, b"/new_dir_sym2", 0o755, 0).map_err(|_| "mkdir after sym ren")?;
                    kassert_eq!(nd.inode_type(), InodeType::Dir);
                }
            }
            Err(_) => return Err("unexpected error in sym rename sweep"),
        }
        drop(guard);
        drop(sym);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (2) Regular file unlink (open)
    let m = mount_test_ext4(base_img.to_vec(), 28)?;
    let (uf, _) = open_inode(
        &m.tables,
        None,
        b"/reg.bin",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create reg.bin")?;
    let pattern = vec![0x5Au8; 4096];
    kassert_eq!(uf.write_at(0, &pattern).map_err(|_| "write reg.bin")?, 4096);
    uf.sync(false).map_err(|_| "sync reg.bin")?;
    drop(uf);
    m.fs.sync().map_err(|_| "sync reg fs")?;
    let snap_reg = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    unlink_at(&m.tables, None, b"/reg.bin", false).map_err(|_| "measure reg unlink")?;
    let reg_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(reg_writes >= 9);
    drop(m);

    for fail_at in 1..=reg_writes {
        let m = mount_test_ext4(snap_reg.clone(), 29)?;
        let uf = resolve_path(&m.tables, None, b"/reg.bin", rf).map_err(|_| "resolve reg.bin")?.inode;
        let guard = OpenGuard::new_unchecked(Arc::clone(&uf));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = unlink_at(&m.tables, None, b"/reg.bin", false);
        m.fault_cache.arm_write_fail_on(0);

        match res {
            Ok(()) => {
                kassert!(uf.is_detached());
                kassert_eq!(read_all_inode(&uf).map_err(|_| "read detached reg ok")?, pattern);
                kassert!(m.fs.lookup_live_by_lock_id(uf.lock_id()).is_none());
            }
            Err(VfsError::Io) => {
                if resolve_path(&m.tables, None, b"/reg.bin", rf).is_ok() {
                    kassert!(!uf.is_detached());
                    kassert_eq!(uf.stat().map_err(|_| "stat reg live")?.nlink, 1);
                    kassert!(m.fs.lookup_live_by_lock_id(uf.lock_id()).is_some());
                    unlink_at(&m.tables, None, b"/reg.bin", false).map_err(|_| "retry reg unlink")?;
                    kassert!(uf.is_detached());
                } else {
                    kassert!(uf.is_detached());
                    kassert_eq!(uf.stat().map_err(|_| "stat reg detached")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(uf.lock_id()).is_none());
                    kassert_eq!(read_all_inode(&uf).map_err(|_| "read detached reg eio")?, pattern);
                    let nd = mkdir_at(&m.tables, None, b"/new_dir_reg", 0o755, 0).map_err(|_| "mkdir after reg")?;
                    kassert_eq!(nd.inode_type(), InodeType::Dir);
                }
            }
            Err(_) => return Err("unexpected error in reg unlink sweep"),
        }
        drop(guard);
        drop(uf);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }
    Ok(())
}

#[inline(never)]
fn sweep_rename_step2_and_exchange_io_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    // (1) Cross-dir file move step-2 sweep
    let m = mount_test_ext4(base_img.to_vec(), 30)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd")?;
    let (sf, _) = open_inode(
        &m.tables,
        None,
        b"/sd/f",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /sd/f")?;
    kassert_eq!(sf.write_at(0, b"FILE_DATA").map_err(|_| "write /sd/f")?, 9);
    drop((sd, dd, sf));
    m.fs.sync().map_err(|_| "sync step2 file")?;
    let snap_f = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default()).map_err(|_| "measure ren file")?;
    let file_ren_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(file_ren_writes >= 4);
    drop(m);

    for fail_at in 1..=file_ren_writes {
        let m = mount_test_ext4(snap_f.clone(), 31)?;
        let f = resolve_path(&m.tables, None, b"/sd/f", rf).map_err(|_| "resolve /sd/f")?.inode;
        let orig_ino = f.ino();
        let guard = OpenGuard::new_unchecked(Arc::clone(&f));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        kassert!(res.is_err() || res.is_ok());

        let old_res = resolve_path(&m.tables, None, b"/sd/f", rf);
        let new_res = resolve_path(&m.tables, None, b"/dd/f", rf);
        match (old_res, new_res) {
            (Ok(_), Err(VfsError::NoEnt)) => {
                kassert!(!f.is_detached());
                kassert_eq!(f.stat().map_err(|_| "stat rolled back f")?.nlink, 1);
                kassert!(m.fs.lookup_live_by_lock_id(f.lock_id()).is_some());
                unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "unlink rolled back f")?;
                kassert!(f.is_detached());
            }
            (Err(VfsError::NoEnt), Ok(n)) => {
                kassert!(!f.is_detached());
                kassert_eq!(n.inode.ino(), orig_ino);
                kassert_eq!(n.inode.lock_id(), f.lock_id());
                kassert_eq!(n.inode.stat().map_err(|_| "stat moved f")?.nlink, 1);
                unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "unlink moved f")?;
                kassert!(f.is_detached());
            }
            (Ok(_), Ok(n)) => {
                kassert!(!f.is_detached());
                kassert_eq!(n.inode.lock_id(), f.lock_id());
                kassert!(f.stat().map_err(|_| "stat dual f")?.nlink >= 2);
                unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "unlink dual f1")?;
                kassert!(!f.is_detached());
                let n2 = resolve_path(&m.tables, None, b"/dd/f", rf).map_err(|_| "still at /dd/f")?.inode;
                kassert_eq!(n2.lock_id(), f.lock_id());
                unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "unlink dual f2")?;
                kassert!(f.is_detached());
            }
            _ => return Err("unexpected file rename step2 state"),
        }
        kassert_eq!(read_all_inode(&f).map_err(|_| "read f detached")?, b"FILE_DATA");
        drop(guard);
        drop(f);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (2) Cross-dir directory move step-2 sweep
    let m = mount_test_ext4(base_img.to_vec(), 32)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd2")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd2")?;
    let sub = mkdir_at(&m.tables, None, b"/sd/sub", 0o755, 0).map_err(|_| "mkdir sub")?;
    drop((sd, dd, sub));
    m.fs.sync().map_err(|_| "sync step2 dir")?;
    let snap_d = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default()).map_err(|_| "measure ren sub")?;
    let dir_ren_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(dir_ren_writes >= 6);
    drop(m);

    for fail_at in 1..=dir_ren_writes {
        let m = mount_test_ext4(snap_d.clone(), 33)?;
        let sd = resolve_path(&m.tables, None, b"/sd", rf).map_err(|_| "resolve sd")?.inode;
        let dd = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "resolve dd")?.inode;
        let sub = resolve_path(&m.tables, None, b"/sd/sub", rf).map_err(|_| "resolve sub")?.inode;
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);

        let ro_fs = mount_ro_snapshot(&m, 34)?;
        let ro_sd = ro_fs.root_inode().lookup("sd").map_err(|_| "ro lookup sd")?;
        let ro_dd = ro_fs.root_inode().lookup("dd").map_err(|_| "ro lookup dd")?;
        let sd_nlink = sd.stat().map_err(|_| "stat sd")?.nlink;
        let dd_nlink = dd.stat().map_err(|_| "stat dd")?.nlink;
        let ro_sd_nlink = ro_sd.stat().map_err(|_| "ro stat sd")?.nlink;
        let ro_dd_nlink = ro_dd.stat().map_err(|_| "ro stat dd")?.nlink;
        if res.is_ok() {
            kassert_eq!(sd_nlink, ro_sd_nlink);
            kassert_eq!(dd_nlink, ro_dd_nlink);
        } else {
            kassert!(sd_nlink <= ro_sd_nlink);
            kassert!(dd_nlink <= ro_dd_nlink);
        }
        drop(ro_fs);

        match (resolve_path(&m.tables, None, b"/sd/sub", rf), resolve_path(&m.tables, None, b"/dd/sub", rf)) {
            (Ok(_), Err(VfsError::NoEnt)) => {
                unlink_at(&m.tables, None, b"/sd/sub", true).map_err(|_| "rmdir rolled back sub")?;
            }
            (Err(VfsError::NoEnt), Ok(_)) => {
                let parent_via_dotdot =
                    resolve_path(&m.tables, None, b"/dd/sub/..", rf).map_err(|_| "resolve /dd/sub/..")?.inode;
                let mut post_mismatches = 0usize;
                if sd.stat().map_err(|_| "stat sd moved")?.nlink != 2 {
                    post_mismatches += 1;
                }
                if parent_via_dotdot.ino() != dd.ino() {
                    post_mismatches += 1;
                }
                if res.is_ok() {
                    kassert_eq!(post_mismatches, 0);
                } else {
                    kassert!(post_mismatches <= 1);
                }
                unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "rmdir moved sub")?;
            }
            (Ok(_), Ok(_)) => {
                kassert!(sub.stat().map_err(|_| "stat dual sub")?.nlink >= 3);
            }
            _ => return Err("unexpected dir rename step2 state"),
        }
        drop((sd, dd, sub, m));
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (3) RENAME_EXCHANGE cross-dir dir <-> inline symlink sweep
    let m = mount_test_ext4(base_img.to_vec(), 35)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd3")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd3")?;
    let da = mkdir_at(&m.tables, None, b"/sd/dir_a", 0o755, 0).map_err(|_| "mkdir dir_a")?;
    let sb = symlink_at(&m.tables, b"/exch/target", None, b"/dd/sym_b").map_err(|_| "symlink sym_b")?;
    drop((sd, dd, da, sb));
    m.fs.sync().map_err(|_| "sync exch")?;
    let snap_ex = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/dir_a", None, b"/dd/sym_b", RenameFlags(RenameFlags::EXCHANGE))
        .map_err(|_| "measure exchange")?;
    let exch_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(exch_writes >= 8);
    drop(m);

    for fail_at in 1..=exch_writes {
        let m = mount_test_ext4(snap_ex.clone(), 36)?;
        let sd = resolve_path(&m.tables, None, b"/sd", rf).map_err(|_| "resolve sd ex")?.inode;
        let dd = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "resolve dd ex")?.inode;
        let da = resolve_path(&m.tables, None, b"/sd/dir_a", rf).map_err(|_| "resolve dir_a")?.inode;
        let sb = resolve_path(&m.tables, None, b"/dd/sym_b", rf).map_err(|_| "resolve sym_b")?.inode;
        let ga = OpenGuard::new_unchecked(Arc::clone(&da));
        let gb = OpenGuard::new_unchecked(Arc::clone(&sb));

        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/dir_a", None, b"/dd/sym_b", RenameFlags(RenameFlags::EXCHANGE));
        m.fault_cache.arm_write_fail_on(0);

        kassert!(!da.is_detached());
        kassert!(!sb.is_detached());
        kassert_eq!(sb.readlink().map_err(|_| "sym_b readlink ex")?, b"/exch/target");

        let ro_fs = mount_ro_snapshot(&m, 37)?;
        let ro_sd = ro_fs.root_inode().lookup("sd").map_err(|_| "ro sd ex")?;
        let ro_dd = ro_fs.root_inode().lookup("dd").map_err(|_| "ro dd ex")?;
        let sd_nlink = sd.stat().map_err(|_| "stat sd ex")?.nlink;
        let dd_nlink = dd.stat().map_err(|_| "stat dd ex")?.nlink;
        let ro_sd_nlink = ro_sd.stat().map_err(|_| "ro stat sd ex")?.nlink;
        let ro_dd_nlink = ro_dd.stat().map_err(|_| "ro stat dd ex")?.nlink;
        if res.is_ok() {
            kassert_eq!(sd_nlink, ro_sd_nlink);
            kassert_eq!(dd_nlink, ro_dd_nlink);
        } else {
            kassert!(sd_nlink <= ro_sd_nlink);
            kassert!(dd_nlink <= ro_dd_nlink);
        }
        drop(ro_fs);

        let a_at_sd = resolve_path(&m.tables, None, b"/sd/dir_a", rf).is_ok_and(|r| r.inode.ino() == da.ino());
        let b_at_dd = resolve_path(&m.tables, None, b"/dd/sym_b", rf).is_ok_and(|r| r.inode.ino() == sb.ino());
        let a_at_dd = resolve_path(&m.tables, None, b"/dd/sym_b", rf).is_ok_and(|r| r.inode.ino() == da.ino());
        let b_at_sd = resolve_path(&m.tables, None, b"/sd/dir_a", rf).is_ok_and(|r| r.inode.ino() == sb.ino());
        if res.is_ok() {
            kassert!(a_at_dd && b_at_sd);
        } else {
            kassert!((a_at_sd && b_at_dd) || (a_at_dd && b_at_sd));
            if a_at_sd && b_at_dd {
                rename_at2(&m.tables, None, b"/sd/dir_a", None, b"/dd/sym_b", RenameFlags(RenameFlags::EXCHANGE))
                    .map_err(|_| "retry exchange")?;
            }
        }
        drop((ga, gb, sd, dd, da, sb, m));
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }
    Ok(())
}

#[inline(never)]
fn sweep_truncate_shrink_io_recovery(base_img: &[u8]) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    let m = mount_test_ext4(base_img.to_vec(), 38)?;
    let (tf, _) = open_inode(
        &m.tables,
        None,
        b"/t8k.bin",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create t8k.bin")?;
    let aa_8k = vec![0xAAu8; 2 * 4096];
    kassert_eq!(tf.write_at(0, &aa_8k).map_err(|_| "write aa_8k")?, aa_8k.len());
    tf.sync(false).map_err(|_| "sync aa_8k")?;
    drop(tf);

    let (sf, _) = open_inode(
        &m.tables,
        None,
        b"/t200.bin",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create t200.bin")?;
    let bb_200 = vec![0xBBu8; 200];
    kassert_eq!(sf.write_at(0, &bb_200).map_err(|_| "write bb_200")?, bb_200.len());
    sf.sync(false).map_err(|_| "sync bb_200")?;
    drop(sf);
    m.fs.sync().map_err(|_| "sync trunc setup")?;
    let snap = m.mem_dev.0.data.lock().clone();

    // Measure clean truncate write counts for 8192 -> 100 and 200 -> 100.
    let tf = resolve_path(&m.tables, None, b"/t8k.bin", rf).map_err(|_| "resolve t8k")?.inode;
    m.fault_cache.arm_write_fail_on(0);
    tf.truncate(100).map_err(|_| "measure t8k")?;
    let t8k_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(t8k_writes >= 7);
    drop(tf);

    let sf = resolve_path(&m.tables, None, b"/t200.bin", rf).map_err(|_| "resolve t200")?.inode;
    m.fault_cache.arm_write_fail_on(0);
    sf.truncate(100).map_err(|_| "measure t200")?;
    let t200_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(t200_writes >= 3);
    drop((sf, m));

    // Fail each block write of the 8 KiB shrink in turn (the tail zeroing and
    // `ext4plus::file::truncate`'s own writes).
    let mut saw_err_at_last_8k = false;
    for fail_at in 1..=t8k_writes {
        let m = mount_test_ext4(snap.clone(), 39)?;
        let tf = resolve_path(&m.tables, None, b"/t8k.bin", rf).map_err(|_| "resolve t8k sweep")?.inode;
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = tf.truncate(100);
        m.fault_cache.arm_write_fail_on(0);

        let ro_fs = mount_ro_snapshot(&m, 40)?;
        let ro_tf = ro_fs.root_inode().lookup("t8k.bin").map_err(|_| "ro t8k")?;
        match res {
            Ok(()) => {
                kassert_eq!(tf.stat().map_err(|_| "stat t8k ok")?.size, 100);
                kassert_eq!(read_all_inode(&tf).map_err(|_| "read t8k ok")?, aa_8k[..100]);
                kassert_eq!(read_all_inode(&ro_tf).map_err(|_| "ro read t8k ok")?, aa_8k[..100]);
            }
            Err(VfsError::Io) => {
                if fail_at == t8k_writes {
                    saw_err_at_last_8k = true;
                }
                kassert_eq!(tf.stat().map_err(|_| "stat t8k eio")?.size, 8192);
                kassert_eq!(read_all_inode(&tf).map_err(|_| "read t8k eio")?, aa_8k);
                kassert_eq!(ro_tf.stat().map_err(|_| "ro stat t8k eio")?.size, 8192);
                kassert_eq!(read_all_inode(&ro_tf).map_err(|_| "ro read t8k eio")?, aa_8k);
            }
            Err(_) => return Err("unexpected error in t8k truncate sweep"),
        }
        drop((ro_fs, tf, m));
    }
    kassert!(saw_err_at_last_8k);

    // Dirty tail in PageCache must also be preserved on ext4_write_at / ext4_truncate EIO.
    for &fail_at in &[1usize, 2usize, t8k_writes] {
        let m = mount_test_ext4(snap.clone(), 43)?;
        let tf = resolve_path(&m.tables, None, b"/t8k.bin", rf).map_err(|_| "resolve t8k dirty")?.inode;
        let cc = [0xCCu8; 32];
        kassert_eq!(tf.write_at(150, &cc).map_err(|_| "dirty write tail")?, cc.len());
        let mut expected = aa_8k.clone();
        expected[150..182].copy_from_slice(&cc);
        m.fault_cache.arm_write_fail_on(fail_at);
        kassert!(matches!(tf.truncate(100), Err(VfsError::Io)));
        m.fault_cache.arm_write_fail_on(0);
        kassert_eq!(tf.stat().map_err(|_| "stat t8k dirty eio")?.size, 8192);
        kassert_eq!(read_all_inode(&tf).map_err(|_| "read t8k dirty eio")?, expected);
        drop((tf, m));
    }

    // Fail each block write of the 200-byte shrink in turn.
    let mut saw_err_at_last_200 = false;
    for fail_at in 1..=t200_writes {
        let m = mount_test_ext4(snap.clone(), 41)?;
        let sf = resolve_path(&m.tables, None, b"/t200.bin", rf).map_err(|_| "resolve t200 sweep")?.inode;
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = sf.truncate(100);
        m.fault_cache.arm_write_fail_on(0);

        let ro_fs = mount_ro_snapshot(&m, 42)?;
        let ro_sf = ro_fs.root_inode().lookup("t200.bin").map_err(|_| "ro t200")?;
        match res {
            Ok(()) => {
                kassert_eq!(sf.stat().map_err(|_| "stat t200 ok")?.size, 100);
                kassert_eq!(read_all_inode(&sf).map_err(|_| "read t200 ok")?, bb_200[..100]);
                kassert_eq!(read_all_inode(&ro_sf).map_err(|_| "ro read t200 ok")?, bb_200[..100]);
            }
            Err(VfsError::Io) => {
                if fail_at == t200_writes {
                    saw_err_at_last_200 = true;
                }
                kassert_eq!(sf.stat().map_err(|_| "stat t200 eio")?.size, 200);
                kassert_eq!(read_all_inode(&sf).map_err(|_| "read t200 eio")?, bb_200);
                kassert_eq!(ro_sf.stat().map_err(|_| "ro stat t200 eio")?.size, 200);
                kassert_eq!(read_all_inode(&ro_sf).map_err(|_| "ro read t200 eio")?, bb_200);
            }
            Err(_) => return Err("unexpected error in t200 truncate sweep"),
        }
        drop((ro_fs, sf, m));
    }
    kassert!(saw_err_at_last_200);
    Ok(())
}

#[inline(never)]
fn sweep_cascading_eio_and_post_res_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;
    use lumen_vfs::link_at;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    // (1) Hardlinked file unlink write-cutoff (From(1..=hl_writes+1), including From(2))
    let m = mount_test_ext4(base_img.to_vec(), 44)?;
    let (hl1, _) = open_inode(
        &m.tables,
        None,
        b"/hl1",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /hl1")?;
    kassert_eq!(hl1.write_at(0, b"HL_DATA").map_err(|_| "write /hl1")?, 7);
    hl1.sync(false).map_err(|_| "sync /hl1")?;
    drop(hl1);
    link_at(&m.tables, None, b"/hl1", None, b"/hl2", false, false).map_err(|_| "link /hl2")?;
    m.fs.sync().map_err(|_| "sync hl")?;
    let snap_hl = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    unlink_at(&m.tables, None, b"/hl1", false).map_err(|_| "measure hl unlink")?;
    let hl_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(hl_writes >= 2);
    drop(m);

    for from_k in 1..=(hl_writes + 1) {
        let m = mount_test_ext4(snap_hl.clone(), 45)?;
        let hl = resolve_path(&m.tables, None, b"/hl1", rf).map_err(|_| "resolve /hl1")?.inode;
        let orig_ino = hl.ino();
        let guard = OpenGuard::new_unchecked(Arc::clone(&hl));

        m.fault_cache.arm_write_fail_from(from_k);
        let res = unlink_at(&m.tables, None, b"/hl1", false);
        m.fault_cache.arm_write_fail_on(0);
        if from_k <= hl_writes {
            kassert!(res.is_err());
        }

        if resolve_path(&m.tables, None, b"/hl1", rf).is_ok() {
            kassert!(!hl.is_detached());
            kassert_eq!(hl.stat().map_err(|_| "stat hl live")?.nlink, 2);
            unlink_at(&m.tables, None, b"/hl1", false).map_err(|_| "unlink /hl1 post cutoff")?;
        }
        kassert!(!hl.is_detached());
        kassert_eq!(hl.stat().map_err(|_| "stat hl remaining")?.nlink, 1);
        let hl2 = resolve_path(&m.tables, None, b"/hl2", rf).map_err(|_| "resolve /hl2 post unlink")?.inode;
        kassert_eq!(hl2.ino(), orig_ino);
        kassert_eq!(hl2.lock_id(), hl.lock_id());
        kassert_eq!(read_all_inode(&hl2).map_err(|_| "read /hl2")?, b"HL_DATA");
        drop(hl2);
        unlink_at(&m.tables, None, b"/hl2", false).map_err(|_| "unlink /hl2")?;
        kassert!(hl.is_detached());
        kassert_eq!(read_all_inode(&hl).map_err(|_| "read detached hl")?, b"HL_DATA");
        drop(guard);
        drop(hl);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (2) Rename step-2 file move: every write failing from k on (From(k)), and every pair of failed writes (Pair(c1, c2)).
    let m = mount_test_ext4(base_img.to_vec(), 46)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd casc f")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd casc f")?;
    let (sf, _) = open_inode(
        &m.tables,
        None,
        b"/sd/f",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /sd/f casc")?;
    kassert_eq!(sf.write_at(0, b"FILE_DATA").map_err(|_| "write /sd/f casc")?, 9);
    drop((sd, dd, sf));
    m.fs.sync().map_err(|_| "sync casc f")?;
    let snap_f = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default()).map_err(|_| "measure casc f")?;
    let file_ren_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(file_ren_writes >= 4);
    drop(m);

    let check_file_step2_post_fault =
        |m: TestExt4Mount, f: Arc<dyn VfsInode>, orig_ino: u64| -> Result<(), &'static str> {
            let guard = OpenGuard::new_unchecked(Arc::clone(&f));
            let old_res = resolve_path(&m.tables, None, b"/sd/f", rf);
            let new_res = resolve_path(&m.tables, None, b"/dd/f", rf);
            match (old_res, new_res) {
                (Ok(_), Err(VfsError::NoEnt)) => {
                    kassert!(!f.is_detached());
                    let nlink = f.stat().map_err(|_| "stat f rb")?.nlink;
                    kassert!(nlink == 1 || nlink == 2);
                    unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "unlink f rb")?;
                    kassert!(matches!(resolve_path(&m.tables, None, b"/sd/f", rf), Err(VfsError::NoEnt)));
                    if nlink == 1 {
                        kassert!(f.is_detached());
                    }
                }
                (Err(VfsError::NoEnt), Ok(n)) => {
                    kassert!(!f.is_detached());
                    kassert_eq!(n.inode.ino(), orig_ino);
                    unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "unlink f mv")?;
                    kassert!(f.is_detached());
                }
                (Ok(_), Ok(n)) => {
                    kassert!(!f.is_detached());
                    kassert_eq!(n.inode.lock_id(), f.lock_id());
                    kassert!(f.stat().map_err(|_| "stat dual f casc")?.nlink >= 2);
                    unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "unlink /sd/f dual casc")?;
                    kassert!(!f.is_detached());
                    kassert_eq!(f.stat().map_err(|_| "stat /dd/f surviving")?.nlink, 1);
                    let dd_f = resolve_path(&m.tables, None, b"/dd/f", rf).map_err(|_| "lookup /dd/f surviving")?.inode;
                    kassert_eq!(dd_f.ino(), orig_ino);
                    kassert_eq!(read_all_inode(&dd_f).map_err(|_| "read /dd/f surviving")?, b"FILE_DATA");
                    let (canary, _) = open_inode(
                        &m.tables,
                        None,
                        b"/canary_f",
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create canary_f")?;
                    kassert_ne_u64(canary.ino(), orig_ino)?;
                    drop((dd_f, canary));
                    unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "unlink /dd/f final")?;
                    kassert!(f.is_detached());
                }
                _ => return Err("unexpected cascading file rename state"),
            }
            kassert_eq!(read_all_inode(&f).map_err(|_| "read f final")?, b"FILE_DATA");
            drop(guard);
            drop(f);
            drop(m);
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
            Ok(())
        };

    for from_k in 1..=(file_ren_writes + 1) {
        let m = mount_test_ext4(snap_f.clone(), 47)?;
        let f = resolve_path(&m.tables, None, b"/sd/f", rf).map_err(|_| "resolve f from")?.inode;
        let orig_ino = f.ino();
        m.fault_cache.arm_write_fail_from(from_k);
        let _ = rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        check_file_step2_post_fault(m, f, orig_ino)?;
    }

    for c1 in 1..=file_ren_writes {
        for c2 in (c1 + 1)..=(c1 + 5) {
            let m = mount_test_ext4(snap_f.clone(), 48)?;
            let f = resolve_path(&m.tables, None, b"/sd/f", rf).map_err(|_| "resolve f pair")?.inode;
            let orig_ino = f.ino();
            m.fault_cache.arm_write_fail_pair(c1, c2);
            let _ = rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default());
            m.fault_cache.arm_write_fail_on(0);
            check_file_step2_post_fault(m, f, orig_ino)?;
        }
    }

    // (3) Rename step-2 dir move: every pair of failed writes (Pair(c1, c2)), and every write failing from k on (From(k)).
    let m = mount_test_ext4(base_img.to_vec(), 49)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd casc d")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd casc d")?;
    let sub = mkdir_at(&m.tables, None, b"/sd/sub", 0o755, 0).map_err(|_| "mkdir sub casc")?;
    drop((sd, dd, sub));
    m.fs.sync().map_err(|_| "sync casc d")?;
    let snap_d = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default()).map_err(|_| "measure casc d")?;
    let dir_ren_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(dir_ren_writes >= 6);
    drop(m);

    let check_dir_step2_post_fault =
        |m: TestExt4Mount, sub: Arc<dyn VfsInode>, orig_ino: u64| -> Result<(), &'static str> {
            match (resolve_path(&m.tables, None, b"/sd/sub", rf), resolve_path(&m.tables, None, b"/dd/sub", rf)) {
                (Ok(_), Err(VfsError::NoEnt)) => {
                    unlink_at(&m.tables, None, b"/sd/sub", true).map_err(|_| "rmdir sub rb")?;
                }
                (Err(VfsError::NoEnt), Ok(_)) => {
                    unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "rmdir sub mv")?;
                }
                (Ok(_), Ok(n)) => {
                    kassert_eq!(n.inode.lock_id(), sub.lock_id());
                    kassert!(sub.stat().map_err(|_| "stat dual sub casc")?.nlink >= 3);
                    unlink_at(&m.tables, None, b"/sd/sub", true).map_err(|_| "rmdir /sd/sub dual")?;
                    kassert!(!sub.is_detached());
                    kassert_eq!(sub.stat().map_err(|_| "stat surviving sub")?.nlink, 2);
                    let dd_sub = resolve_path(&m.tables, None, b"/dd/sub", rf).map_err(|_| "lookup /dd/sub")?.inode;
                    kassert_eq!(dd_sub.ino(), orig_ino);
                    let canary_d = mkdir_at(&m.tables, None, b"/canary_d", 0o755, 0).map_err(|_| "mkdir canary_d")?;
                    kassert_ne_u64(canary_d.ino(), orig_ino)?;
                    drop((dd_sub, canary_d));
                    unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "rmdir /dd/sub final")?;
                }
                _ => return Err("unexpected cascading dir rename state"),
            }
            drop((sub, m));
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
            Ok(())
        };

    for from_k in 1..=(dir_ren_writes + 1) {
        let m = mount_test_ext4(snap_d.clone(), 50)?;
        let sub = resolve_path(&m.tables, None, b"/sd/sub", rf).map_err(|_| "resolve sub from")?.inode;
        let orig_ino = sub.ino();
        m.fault_cache.arm_write_fail_from(from_k);
        let _ = rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        check_dir_step2_post_fault(m, sub, orig_ino)?;
    }

    for c1 in 1..=dir_ren_writes {
        for c2 in (c1 + 1)..=(c1 + 5) {
            let m = mount_test_ext4(snap_d.clone(), 51)?;
            let sub = resolve_path(&m.tables, None, b"/sd/sub", rf).map_err(|_| "resolve sub pair")?.inode;
            let orig_ino = sub.ino();
            m.fault_cache.arm_write_fail_pair(c1, c2);
            let _ = rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default());
            m.fault_cache.arm_write_fail_on(0);
            check_dir_step2_post_fault(m, sub, orig_ino)?;
        }
    }

    // (4) RENAME_EXCHANGE file<->file: each single failed write's post_res checks, then pairs of failed writes.
    let m = mount_test_ext4(base_img.to_vec(), 52)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd ex f")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd ex f")?;
    let (fa, _) = open_inode(
        &m.tables,
        None,
        b"/sd/fa",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /sd/fa")?;
    let (fb, _) = open_inode(
        &m.tables,
        None,
        b"/dd/fb",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create /dd/fb")?;
    kassert_eq!(fa.write_at(0, b"FA_DATA").map_err(|_| "write /sd/fa")?, 7);
    kassert_eq!(fb.write_at(0, b"FB_DATA").map_err(|_| "write /dd/fb")?, 7);
    drop((sd, dd, fa, fb));
    m.fs.sync().map_err(|_| "sync ex f")?;
    let snap_ex_f = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/fa", None, b"/dd/fb", RenameFlags(RenameFlags::EXCHANGE))
        .map_err(|_| "measure ex f")?;
    let exch_file_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(exch_file_writes >= 10);
    drop(m);

    // Single-write post_res verification on file<->file RENAME_EXCHANGE
    for fail_at in 1..=exch_file_writes {
        let m = mount_test_ext4(snap_ex_f.clone(), 53)?;
        let fa = resolve_path(&m.tables, None, b"/sd/fa", rf).map_err(|_| "resolve fa single")?.inode;
        let fb = resolve_path(&m.tables, None, b"/dd/fb", rf).map_err(|_| "resolve fb single")?.inode;
        let ga = OpenGuard::new_unchecked(Arc::clone(&fa));
        let gb = OpenGuard::new_unchecked(Arc::clone(&fb));
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/fa", None, b"/dd/fb", RenameFlags(RenameFlags::EXCHANGE));
        m.fault_cache.arm_write_fail_on(0);

        let fa_at_dd = resolve_path(&m.tables, None, b"/dd/fb", rf).is_ok_and(|r| r.inode.ino() == fa.ino());
        let fb_at_sd = resolve_path(&m.tables, None, b"/sd/fa", rf).is_ok_and(|r| r.inode.ino() == fb.ino());
        if fa_at_dd && fb_at_sd {
            // Post-commit swap: at most one of old_disk.write / new_disk.write could have failed.
            let fa_nlink = fa.stat().map_err(|_| "stat fa post")?.nlink;
            let fb_nlink = fb.stat().map_err(|_| "stat fb post")?.nlink;
            if res.is_ok() {
                kassert_eq!(fa_nlink, 1);
                kassert_eq!(fb_nlink, 1);
            } else {
                kassert!(fa_nlink == 1 || fb_nlink == 1);
                if fb_nlink == 1 {
                    unlink_at(&m.tables, None, b"/sd/fa", false).map_err(|_| "unlink swapped fb")?;
                    kassert!(fb.is_detached());
                } else {
                    unlink_at(&m.tables, None, b"/dd/fb", false).map_err(|_| "unlink swapped fa")?;
                    kassert!(fa.is_detached());
                }
            }
        }
        drop((ga, gb, fa, fb, m));
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // Pairs of failed writes (c2 up to 6 writes after c1) on file<->file RENAME_EXCHANGE.
    for &pinned in &[false, true] {
        for c1 in 1..=exch_file_writes {
            for c2 in (c1 + 1)..=(c1 + 6) {
                let m = mount_test_ext4(snap_ex_f.clone(), 54)?;
                let (fa_orig_ino, fb_orig_ino, pinned_pair) = if pinned {
                    let fa = resolve_path(&m.tables, None, b"/sd/fa", rf).map_err(|_| "resolve fa pair")?.inode;
                    let fb = resolve_path(&m.tables, None, b"/dd/fb", rf).map_err(|_| "resolve fb pair")?.inode;
                    let a_ino = fa.ino();
                    let b_ino = fb.ino();
                    let ga = OpenGuard::new_unchecked(Arc::clone(&fa));
                    let gb = OpenGuard::new_unchecked(Arc::clone(&fb));
                    (a_ino, b_ino, Some((fa, fb, ga, gb)))
                } else {
                    let ro_fs = mount_ro_snapshot(&m, 54)?;
                    let a_ino =
                        ro_fs.root_inode().lookup("sd").and_then(|d| d.lookup("fa")).map_err(|_| "ro fa pair")?.ino();
                    let b_ino =
                        ro_fs.root_inode().lookup("dd").and_then(|d| d.lookup("fb")).map_err(|_| "ro fb pair")?.ino();
                    (a_ino, b_ino, None)
                };

                m.fault_cache.arm_write_fail_pair(c1, c2);
                let _ = rename_at2(&m.tables, None, b"/sd/fa", None, b"/dd/fb", RenameFlags(RenameFlags::EXCHANGE));
                m.fault_cache.arm_write_fail_on(0);

                let fa_at_sd = resolve_path(&m.tables, None, b"/sd/fa", rf).is_ok_and(|r| r.inode.ino() == fa_orig_ino);
                let fa_at_dd = resolve_path(&m.tables, None, b"/dd/fb", rf).is_ok_and(|r| r.inode.ino() == fa_orig_ino);
                let fb_at_dd = resolve_path(&m.tables, None, b"/dd/fb", rf).is_ok_and(|r| r.inode.ino() == fb_orig_ino);
                let fb_at_sd = resolve_path(&m.tables, None, b"/sd/fa", rf).is_ok_and(|r| r.inode.ino() == fb_orig_ino);

                // Unconditionally assert BOTH original inodes remain reachable at distinct slots.
                kassert!((fa_at_sd && fb_at_dd) || (fa_at_dd && fb_at_sd));
                let cur_sd = resolve_path(&m.tables, None, b"/sd/fa", rf).map_err(|_| "resolve post /sd/fa")?.inode;
                let cur_dd = resolve_path(&m.tables, None, b"/dd/fb", rf).map_err(|_| "resolve post /dd/fb")?.inode;
                let (cur_fa, cur_fb) = if fa_at_sd { (cur_sd, cur_dd) } else { (cur_dd, cur_sd) };
                kassert!(!cur_fa.is_detached());
                kassert!(!cur_fb.is_detached());
                kassert_eq!(cur_fa.stat().map_err(|_| "stat post fa")?.nlink, 1);
                kassert_eq!(cur_fb.stat().map_err(|_| "stat post fb")?.nlink, 1);
                kassert_eq!(read_all_inode(&cur_fa).map_err(|_| "read fa pair")?, b"FA_DATA");
                kassert_eq!(read_all_inode(&cur_fb).map_err(|_| "read fb pair")?, b"FB_DATA");

                let g_cur_a = OpenGuard::new_unchecked(Arc::clone(&cur_fa));
                let g_cur_b = OpenGuard::new_unchecked(Arc::clone(&cur_fb));
                unlink_at(&m.tables, None, b"/sd/fa", false).map_err(|_| "unlink post /sd/fa")?;
                unlink_at(&m.tables, None, b"/dd/fb", false).map_err(|_| "unlink post /dd/fb")?;
                kassert!(cur_fa.is_detached());
                kassert!(cur_fb.is_detached());
                kassert_eq!(cur_fa.stat().map_err(|_| "stat detached fa")?.nlink, 0);
                kassert_eq!(cur_fb.stat().map_err(|_| "stat detached fb")?.nlink, 0);
                kassert!(m.fs.lookup_live_by_lock_id(cur_fa.lock_id()).is_none());
                kassert!(m.fs.lookup_live_by_lock_id(cur_fb.lock_id()).is_none());
                drop((g_cur_a, g_cur_b, cur_fa, cur_fb, pinned_pair));

                let (ra, _) = open_inode(
                    &m.tables,
                    None,
                    b"/reuse_ex_fa",
                    OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                    0o644,
                    0,
                )
                .map_err(|_| "reuse ex fa")?;
                let (rb, _) = open_inode(
                    &m.tables,
                    None,
                    b"/reuse_ex_fb",
                    OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                    0o644,
                    0,
                )
                .map_err(|_| "reuse ex fb")?;
                let r1 = ra.ino();
                let r2 = rb.ino();
                kassert!((r1 == fa_orig_ino && r2 == fb_orig_ino) || (r1 == fb_orig_ino && r2 == fa_orig_ino));
                drop((ra, rb, m));
                kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
            }
        }
    }

    // (5) RENAME_EXCHANGE dir<->dir single-write post_res verification across 1..=total_writes
    let m = mount_test_ext4(base_img.to_vec(), 55)?;
    let sd = mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd ex d")?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd ex d")?;
    let da = mkdir_at(&m.tables, None, b"/sd/da", 0o755, 0).map_err(|_| "mkdir /sd/da")?;
    let db = mkdir_at(&m.tables, None, b"/dd/db", 0o755, 0).map_err(|_| "mkdir /dd/db")?;
    drop((sd, dd, da, db));
    m.fs.sync().map_err(|_| "sync ex d")?;
    let snap_ex_d = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/da", None, b"/dd/db", RenameFlags(RenameFlags::EXCHANGE))
        .map_err(|_| "measure ex d")?;
    let exch_dir_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(exch_dir_writes >= 12);
    drop(m);

    for fail_at in 1..=exch_dir_writes {
        let m = mount_test_ext4(snap_ex_d.clone(), 56)?;
        let sd = resolve_path(&m.tables, None, b"/sd", rf).map_err(|_| "resolve sd ex d")?.inode;
        let dd = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "resolve dd ex d")?.inode;
        let da = resolve_path(&m.tables, None, b"/sd/da", rf).map_err(|_| "resolve da ex d")?.inode;
        let db = resolve_path(&m.tables, None, b"/dd/db", rf).map_err(|_| "resolve db ex d")?.inode;

        let orig_da_ino = da.ino();
        let orig_db_ino = db.ino();
        let ga = OpenGuard::new_unchecked(Arc::clone(&da));
        let gb = OpenGuard::new_unchecked(Arc::clone(&db));

        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/sd/da", None, b"/dd/db", RenameFlags(RenameFlags::EXCHANGE));
        m.fault_cache.arm_write_fail_on(0);

        let ro_fs = mount_ro_snapshot(&m, 57)?;
        let ro_sd = ro_fs.root_inode().lookup("sd").map_err(|_| "ro sd ex d")?;
        let ro_dd = ro_fs.root_inode().lookup("dd").map_err(|_| "ro dd ex d")?;
        let sd_nlink = sd.stat().map_err(|_| "stat sd ex d")?.nlink;
        let dd_nlink = dd.stat().map_err(|_| "stat dd ex d")?.nlink;
        let ro_sd_nlink = ro_sd.stat().map_err(|_| "ro stat sd ex d")?.nlink;
        let ro_dd_nlink = ro_dd.stat().map_err(|_| "ro stat dd ex d")?.nlink;
        if res.is_ok() {
            kassert_eq!(sd_nlink, ro_sd_nlink);
            kassert_eq!(dd_nlink, ro_dd_nlink);
        } else {
            kassert!(sd_nlink <= ro_sd_nlink);
            kassert!(dd_nlink <= ro_dd_nlink);
        }
        drop(ro_fs);

        let da_at_dd = resolve_path(&m.tables, None, b"/dd/db", rf).is_ok_and(|r| r.inode.ino() == orig_da_ino);
        let db_at_sd = resolve_path(&m.tables, None, b"/sd/da", rf).is_ok_and(|r| r.inode.ino() == orig_db_ino);
        if da_at_dd && db_at_sd {
            let da_dotdot = resolve_path(&m.tables, None, b"/dd/db/..", rf).map_err(|_| "resolve da ..")?.inode;
            let db_dotdot = resolve_path(&m.tables, None, b"/sd/da/..", rf).map_err(|_| "resolve db ..")?.inode;
            let mut dotdot_mismatches = 0usize;
            kassert_eq!(da.stat().map_err(|_| "stat da post")?.nlink, 2);
            kassert_eq!(db.stat().map_err(|_| "stat db post")?.nlink, 2);
            kassert_eq!(sd.stat().map_err(|_| "stat sd post")?.nlink, 3);
            kassert_eq!(dd.stat().map_err(|_| "stat dd post")?.nlink, 3);
            if da_dotdot.ino() != dd.ino() {
                dotdot_mismatches += 1;
            }
            if db_dotdot.ino() != sd.ino() {
                dotdot_mismatches += 1;
            }
            if res.is_ok() {
                kassert_eq!(dotdot_mismatches, 0);
            } else {
                kassert!(dotdot_mismatches <= 1);
            }
            drop((da_dotdot, db_dotdot));
            unlink_at(&m.tables, None, b"/dd/db", true).map_err(|_| "rmdir swapped da")?;
            unlink_at(&m.tables, None, b"/sd/da", true).map_err(|_| "rmdir swapped db")?;
            kassert!(da.is_detached());
            kassert!(db.is_detached());
            kassert_eq!(da.stat().map_err(|_| "stat da detached")?.nlink, 0);
            kassert_eq!(db.stat().map_err(|_| "stat db detached")?.nlink, 0);
            kassert!(m.fs.lookup_live_by_lock_id(da.lock_id()).is_none());
            kassert!(m.fs.lookup_live_by_lock_id(db.lock_id()).is_none());
            drop((ga, gb, da, db));
            let reuse_a = mkdir_at(&m.tables, None, b"/reuse_ex_a", 0o755, 0).map_err(|_| "mkdir reuse_ex_a")?.ino();
            let reuse_b = mkdir_at(&m.tables, None, b"/reuse_ex_b", 0o755, 0).map_err(|_| "mkdir reuse_ex_b")?.ino();
            kassert!(
                (reuse_a == orig_da_ino && reuse_b == orig_db_ino)
                    || (reuse_a == orig_db_ino && reuse_b == orig_da_ino)
            );
            drop((sd, dd, m));
        } else {
            drop((ga, gb, sd, dd, da, db, m));
        }
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6) Cold-cache (open_count == 0, NO OpenGuard / no prior resolve_path) Pair(2, 3) & From(2)
    //     regression tests across hardlinked files, hardlinked inline symlinks, hardlink
    //     rename overwrites, dual-linked directories, and uncached step-2 file rename.

    // (6a) Cold-cache hardlinked regular file unlink_at("/h1")
    let m = mount_test_ext4(base_img.to_vec(), 58)?;
    let (h1, _) = open_inode(
        &m.tables,
        None,
        b"/h1",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create cold /h1")?;
    let cold_hl_orig_ino = h1.ino();
    kassert_eq!(h1.write_at(0, b"COLD_HL_DATA").map_err(|_| "write cold /h1")?, 12);
    h1.sync(false).map_err(|_| "sync cold /h1")?;
    drop(h1);
    link_at(&m.tables, None, b"/h1", None, b"/h2", false, false).map_err(|_| "link cold /h2")?;
    m.fs.sync().map_err(|_| "sync cold hl")?;
    let snap_cold_hl = m.mem_dev.0.data.lock().clone();
    drop(m);

    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_cold_hl.clone(), 59)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(2);
        } else {
            m.fault_cache.arm_write_fail_pair(2, 3);
        }
        let res1 = unlink_at(&m.tables, None, b"/h1", false);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res1, Err(VfsError::Io)));

        unlink_at(&m.tables, None, b"/h1", false).map_err(|_| "retry cold /h1 unlink")?;
        kassert!(matches!(resolve_path(&m.tables, None, b"/h1", rf), Err(VfsError::NoEnt)));
        let h2 = resolve_path(&m.tables, None, b"/h2", rf).map_err(|_| "resolve cold /h2")?.inode;
        kassert_eq!(h2.ino(), cold_hl_orig_ino);
        kassert_eq!(h2.stat().map_err(|_| "stat cold /h2")?.nlink, 1);
        kassert_eq!(read_all_inode(&h2).map_err(|_| "read cold /h2")?, b"COLD_HL_DATA");
        let (canary, _) = open_inode(
            &m.tables,
            None,
            b"/canary",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create cold /canary")?;
        kassert_ne_u64(canary.ino(), cold_hl_orig_ino)?;
        drop((h2, canary));
        unlink_at(&m.tables, None, b"/h2", false).map_err(|_| "cleanup cold /h2")?;
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6b) Cold-cache hardlinked inline symlink unlink_at("/sym1")
    let m = mount_test_ext4(base_img.to_vec(), 60)?;
    let sym1 = symlink_at(&m.tables, b"/cold/sym/target", None, b"/sym1").map_err(|_| "symlink cold /sym1")?;
    let cold_sym_orig_ino = sym1.ino();
    drop(sym1);
    link_at(&m.tables, None, b"/sym1", None, b"/sym2", false, false).map_err(|_| "link cold /sym2")?;
    m.fs.sync().map_err(|_| "sync cold sym")?;
    let snap_cold_sym = m.mem_dev.0.data.lock().clone();
    drop(m);

    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_cold_sym.clone(), 61)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(2);
        } else {
            m.fault_cache.arm_write_fail_pair(2, 3);
        }
        let res1 = unlink_at(&m.tables, None, b"/sym1", false);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res1, Err(VfsError::Io)));

        unlink_at(&m.tables, None, b"/sym1", false).map_err(|_| "retry cold /sym1 unlink")?;
        kassert!(matches!(resolve_path(&m.tables, None, b"/sym1", rf), Err(VfsError::NoEnt)));
        let sym2 = resolve_path(&m.tables, None, b"/sym2", rf).map_err(|_| "resolve cold /sym2")?.inode;
        kassert_eq!(sym2.ino(), cold_sym_orig_ino);
        kassert_eq!(sym2.stat().map_err(|_| "stat cold /sym2")?.nlink, 1);
        kassert_eq!(sym2.readlink().map_err(|_| "readlink cold /sym2")?, b"/cold/sym/target");
        kassert_eq!(readlink_at(&m.tables, None, b"/sym2").map_err(|_| "readlink_at cold /sym2")?, b"/cold/sym/target");
        let canary_sym = symlink_at(&m.tables, b"/canary/target", None, b"/canary_sym").map_err(|_| "canary_sym")?;
        kassert_ne_u64(canary_sym.ino(), cold_sym_orig_ino)?;
        drop((sym2, canary_sym));
        unlink_at(&m.tables, None, b"/sym2", false).map_err(|_| "cleanup cold /sym2")?;
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6c) Cold-cache hardlink overwrite rename_at2("/src", "/h1")
    let m = mount_test_ext4(base_img.to_vec(), 62)?;
    let (ow_h1, _) = open_inode(
        &m.tables,
        None,
        b"/h1",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create cold ow /h1")?;
    let cold_ow_orig_ino = ow_h1.ino();
    kassert_eq!(ow_h1.write_at(0, b"OLD_HL_OW").map_err(|_| "write cold ow /h1")?, 9);
    ow_h1.sync(false).map_err(|_| "sync cold ow /h1")?;
    drop(ow_h1);
    link_at(&m.tables, None, b"/h1", None, b"/h2", false, false).map_err(|_| "link cold ow /h2")?;
    let (ow_src, _) = open_inode(
        &m.tables,
        None,
        b"/src",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create cold ow /src")?;
    let ow_src_ino = ow_src.ino();
    kassert_eq!(ow_src.write_at(0, b"NEW_SRC_OW").map_err(|_| "write cold ow /src")?, 10);
    ow_src.sync(false).map_err(|_| "sync cold ow /src")?;
    drop(ow_src);
    m.fs.sync().map_err(|_| "sync cold ow")?;
    let snap_cold_ow = m.mem_dev.0.data.lock().clone();
    drop(m);

    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_cold_ow.clone(), 63)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(2);
        } else {
            m.fault_cache.arm_write_fail_pair(2, 3);
        }
        let res1 = rename_at2(&m.tables, None, b"/src", None, b"/h1", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res1, Err(VfsError::Io)));

        rename_at2(&m.tables, None, b"/src", None, b"/h1", RenameFlags::default())
            .map_err(|_| "retry cold ow rename")?;
        let new_h1 = resolve_path(&m.tables, None, b"/h1", rf).map_err(|_| "resolve new cold ow /h1")?.inode;
        kassert_eq!(new_h1.ino(), ow_src_ino);
        kassert_eq!(read_all_inode(&new_h1).map_err(|_| "read new cold ow /h1")?, b"NEW_SRC_OW");
        let h2 = resolve_path(&m.tables, None, b"/h2", rf).map_err(|_| "resolve cold ow /h2")?.inode;
        kassert_eq!(h2.ino(), cold_ow_orig_ino);
        kassert_eq!(h2.stat().map_err(|_| "stat cold ow /h2")?.nlink, 1);
        kassert_eq!(read_all_inode(&h2).map_err(|_| "read cold ow /h2")?, b"OLD_HL_OW");
        let (canary_ow, _) = open_inode(
            &m.tables,
            None,
            b"/canary_ow",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create cold /canary_ow")?;
        kassert_ne_u64(canary_ow.ino(), cold_ow_orig_ino)?;
        drop((new_h1, h2, canary_ow));
        unlink_at(&m.tables, None, b"/h2", false).map_err(|_| "cleanup cold ow /h2")?;
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6d) Cold-cache dual-linked empty directory unlink_at("/sd/sub", true)
    let mut snap_dual_dir_opt: Option<(Vec<u8>, u64)> = None;
    'find_dual: for c1 in 1..=dir_ren_writes {
        for c2 in (c1 + 1)..=(c1 + 5) {
            let m = mount_test_ext4(snap_d.clone(), 64)?;
            m.fault_cache.arm_write_fail_pair(c1, c2);
            let _ = rename_at2(&m.tables, None, b"/sd/sub", None, b"/dd/sub", RenameFlags::default());
            m.fault_cache.arm_write_fail_on(0);
            if let (Ok(sd_r), Ok(dd_r)) =
                (resolve_path(&m.tables, None, b"/sd/sub", rf), resolve_path(&m.tables, None, b"/dd/sub", rf))
            {
                let ino = sd_r.inode.ino();
                if dd_r.inode.ino() == ino {
                    drop((sd_r, dd_r));
                    let ro_fs = mount_ro_snapshot(&m, 65)?;
                    let ro_sub = ro_fs
                        .root_inode()
                        .lookup("dd")
                        .and_then(|d| d.lookup("sub"))
                        .map_err(|_| "ro lookup dual sub")?;
                    if ro_sub.stat().map_err(|_| "ro stat dual sub")?.nlink >= 3 {
                        drop(ro_sub);
                        drop(ro_fs);
                        m.fs.sync().map_err(|_| "sync dual dir snap")?;
                        snap_dual_dir_opt = Some((m.mem_dev.0.data.lock().clone(), ino));
                        drop(m);
                        break 'find_dual;
                    }
                }
            }
            drop(m);
        }
    }
    let (snap_dual_dir, dual_dir_orig_ino) = snap_dual_dir_opt.ok_or("missing dual dir snapshot")?;

    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_dual_dir.clone(), 66)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(2);
        } else {
            m.fault_cache.arm_write_fail_pair(2, 3);
        }
        let res1 = unlink_at(&m.tables, None, b"/sd/sub", true);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res1, Err(VfsError::Io)));

        unlink_at(&m.tables, None, b"/sd/sub", true).map_err(|_| "retry cold dual rmdir")?;
        kassert!(matches!(resolve_path(&m.tables, None, b"/sd/sub", rf), Err(VfsError::NoEnt)));
        let dd_sub = resolve_path(&m.tables, None, b"/dd/sub", rf).map_err(|_| "resolve cold /dd/sub")?.inode;
        kassert_eq!(dd_sub.ino(), dual_dir_orig_ino);
        kassert_eq!(dd_sub.stat().map_err(|_| "stat cold /dd/sub")?.nlink, 2);
        let canary_dir = mkdir_at(&m.tables, None, b"/canary_dir", 0o755, 0).map_err(|_| "mkdir cold /canary_dir")?;
        kassert_ne_u64(canary_dir.ino(), dual_dir_orig_ino)?;
        drop((dd_sub, canary_dir));
        unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "cleanup cold /dd/sub")?;
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (6e) Cold-cache step-2 file rename From(4) / Pair(3, 4) without prior resolve_path
    let snap_f_orig_ino = {
        let ro_dev = MemBlkDev::new(snap_f.clone(), true);
        let ro_bc: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(lumen_bcache::BlockCache::new(ro_dev, 32).map_err(|_| "ro bc snap_f")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bc, DevId::makedev(0, 67), true).map_err(|_| "ro snap_f")?;
        ro_fs.root_inode().lookup("sd").and_then(|d| d.lookup("f")).map_err(|_| "ro /sd/f")?.ino()
    };
    for &use_from in &[false, true] {
        let m = mount_test_ext4(snap_f.clone(), 68)?;
        if use_from {
            m.fault_cache.arm_write_fail_from(4);
        } else {
            m.fault_cache.arm_write_fail_pair(3, 4);
        }
        let _ = rename_at2(&m.tables, None, b"/sd/f", None, b"/dd/f", RenameFlags::default());
        m.fault_cache.arm_write_fail_on(0);
        if resolve_path(&m.tables, None, b"/sd/f", rf).is_ok() && resolve_path(&m.tables, None, b"/dd/f", rf).is_ok() {
            unlink_at(&m.tables, None, b"/sd/f", false).map_err(|_| "cold s2 unlink /sd/f")?;
            let dd_f = resolve_path(&m.tables, None, b"/dd/f", rf).map_err(|_| "cold s2 resolve /dd/f")?.inode;
            kassert_eq!(dd_f.ino(), snap_f_orig_ino);
            kassert_eq!(read_all_inode(&dd_f).map_err(|_| "cold s2 read /dd/f")?, b"FILE_DATA");
            let (canary_s2, _) = open_inode(
                &m.tables,
                None,
                b"/canary_s2",
                OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                0o644,
                0,
            )
            .map_err(|_| "create canary_s2")?;
            kassert_ne_u64(canary_s2.ino(), snap_f_orig_ino)?;
            drop((dd_f, canary_s2));
            unlink_at(&m.tables, None, b"/dd/f", false).map_err(|_| "cleanup cold s2 /dd/f")?;
        }
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    Ok(())
}

#[inline(never)]
fn sweep_upward_drift_and_dir_floor_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    let ro_ino_at = |img: &[u8], dir_name: Option<&str>, name: &str, minor: u32| -> Result<u64, &'static str> {
        let ro_dev = MemBlkDev::new(img.to_vec(), true);
        let ro_bc: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(lumen_bcache::BlockCache::new(ro_dev, 32).map_err(|_| "ro bc")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bc, DevId::makedev(0, minor), true).map_err(|_| "ro mount")?;
        let dir = match dir_name {
            Some(d) => ro_fs.root_inode().lookup(d).map_err(|_| "ro lookup parent")?,
            None => ro_fs.root_inode(),
        };
        Ok(dir.lookup(name).map_err(|_| "ro lookup child")?.ino())
    };

    // (A) rename_at2 step-2 Pair(2,3)/From(2) (and Pair(3,4)/From(3) for dir), cold-cache & pinned,
    //     cross-dir & same-dir, across regular file, inline symlink, and directory.
    #[derive(Clone, Copy)]
    enum EntryKind {
        File,
        Symlink,
        Dir,
    }
    for &kind in &[EntryKind::File, EntryKind::Symlink, EntryKind::Dir] {
        for &same_dir in &[false, true] {
            let m = mount_test_ext4(base_img.to_vec(), 70)?;
            mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd A")?;
            if !same_dir {
                mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd A")?;
            }
            let src_path: &[u8] = b"/sd/item";
            let dst_path: &[u8] = if same_dir { b"/sd/item2" } else { b"/dd/item" };
            match kind {
                EntryKind::File => {
                    let (f, _) = open_inode(
                        &m.tables,
                        None,
                        src_path,
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create src file A")?;
                    kassert_eq!(f.write_at(0, b"UP_FILE").map_err(|_| "write src file A")?, 7);
                }
                EntryKind::Symlink => {
                    symlink_at(&m.tables, b"/up/sym/target", None, src_path).map_err(|_| "symlink src A")?;
                }
                EntryKind::Dir => {
                    mkdir_at(&m.tables, None, src_path, 0o755, 0).map_err(|_| "mkdir src dir A")?;
                }
            }
            m.fs.sync().map_err(|_| "sync A")?;
            let snap = m.mem_dev.0.data.lock().clone();
            let orig_ino = ro_ino_at(&snap, Some("sd"), "item", 71)?;
            drop(m);

            let faults: &[(usize, usize)] = match kind {
                EntryKind::Dir => &[(2, 3), (3, 4), (2, 0), (3, 0)],
                _ => &[(2, 3), (2, 0)],
            };
            for &pinned in &[false, true] {
                for &(c1, c2) in faults {
                    let m = mount_test_ext4(snap.clone(), 72)?;
                    let pinned_inode = if pinned {
                        Some(resolve_path(&m.tables, None, src_path, rf).map_err(|_| "pin src A")?.inode)
                    } else {
                        None
                    };
                    let pinned_guard = pinned_inode.as_ref().map(|n| OpenGuard::new_unchecked(Arc::clone(n)));

                    if c2 == 0 {
                        m.fault_cache.arm_write_fail_from(c1);
                    } else {
                        m.fault_cache.arm_write_fail_pair(c1, c2);
                    }
                    let res = rename_at2(&m.tables, None, src_path, None, dst_path, RenameFlags::default());
                    m.fault_cache.arm_write_fail_on(0);
                    kassert!(matches!(res, Err(VfsError::Io)));

                    let src_ok = resolve_path(&m.tables, None, src_path, rf).ok().map(|r| r.inode);
                    let dst_ok = resolve_path(&m.tables, None, dst_path, rf).ok().map(|r| r.inode);
                    let target = src_ok.as_ref().or(dst_ok.as_ref()).ok_or("missing item A")?;
                    kassert_eq!(target.ino(), orig_ino);
                    let g = OpenGuard::new_unchecked(Arc::clone(target));
                    let is_dir = matches!(kind, EntryKind::Dir);
                    if src_ok.is_some() && dst_ok.is_some() {
                        unlink_at(&m.tables, None, src_path, is_dir).map_err(|_| "unlink src dual A")?;
                        kassert!(!target.is_detached());
                        unlink_at(&m.tables, None, dst_path, is_dir).map_err(|_| "unlink dst dual A")?;
                    } else if src_ok.is_some() {
                        unlink_at(&m.tables, None, src_path, is_dir).map_err(|_| "unlink src A")?;
                    } else {
                        unlink_at(&m.tables, None, dst_path, is_dir).map_err(|_| "unlink dst A")?;
                    }
                    kassert!(target.is_detached());
                    kassert_eq!(target.stat().map_err(|_| "stat detached A")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(target.lock_id()).is_none());
                    let target_clone = Arc::clone(target);
                    drop((g, src_ok, dst_ok, pinned_guard, pinned_inode));
                    drop(target_clone);

                    let reuse_ino = match kind {
                        EntryKind::File => open_inode(
                            &m.tables,
                            None,
                            b"/reuse_a",
                            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                            0o644,
                            0,
                        )
                        .map_err(|_| "reuse file A")?
                        .0
                        .ino(),
                        EntryKind::Symlink => {
                            symlink_at(&m.tables, b"/t", None, b"/reuse_a").map_err(|_| "reuse sym A")?.ino()
                        }
                        EntryKind::Dir => {
                            mkdir_at(&m.tables, None, b"/reuse_a", 0o755, 0).map_err(|_| "reuse dir A")?.ino()
                        }
                    };
                    kassert_eq!(reuse_ino, orig_ino);
                    drop(m);
                    kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
                }
            }
        }
    }

    // (B) RENAME_EXCHANGE Pair(2,3)/From(2), cold-cache & pinned, cross-dir & same-dir,
    //     verifying detach (is_detached, nlink == 0, live_inodes cleared) and exact inode reuse.
    struct ExCase {
        same_dir: bool,
        a_kind: EntryKind,
        b_kind: EntryKind,
    }
    let ex_cases = [
        ExCase { same_dir: false, a_kind: EntryKind::File, b_kind: EntryKind::File },
        ExCase { same_dir: false, a_kind: EntryKind::Dir, b_kind: EntryKind::Symlink },
        ExCase { same_dir: false, a_kind: EntryKind::Dir, b_kind: EntryKind::Dir },
        ExCase { same_dir: true, a_kind: EntryKind::File, b_kind: EntryKind::File },
        ExCase { same_dir: true, a_kind: EntryKind::Dir, b_kind: EntryKind::File },
    ];
    for case in &ex_cases {
        let m = mount_test_ext4(base_img.to_vec(), 73)?;
        mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd B")?;
        if !case.same_dir {
            mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd B")?;
        }
        let path_a: &[u8] = b"/sd/ea";
        let path_b: &[u8] = if case.same_dir { b"/sd/eb" } else { b"/dd/eb" };
        let make_entry = |path: &[u8], kind: EntryKind| -> Result<(), &'static str> {
            match kind {
                EntryKind::File => {
                    let (f, _) = open_inode(
                        &m.tables,
                        None,
                        path,
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create ex B")?;
                    kassert_eq!(f.write_at(0, b"EX_DATA").map_err(|_| "write ex B")?, 7);
                }
                EntryKind::Symlink => {
                    symlink_at(&m.tables, b"/ex/sym/target", None, path).map_err(|_| "sym ex B")?;
                }
                EntryKind::Dir => {
                    mkdir_at(&m.tables, None, path, 0o755, 0).map_err(|_| "mkdir ex B")?;
                }
            }
            Ok(())
        };
        make_entry(path_a, case.a_kind)?;
        make_entry(path_b, case.b_kind)?;
        m.fs.sync().map_err(|_| "sync B")?;
        let snap = m.mem_dev.0.data.lock().clone();
        let orig_a_ino = ro_ino_at(&snap, Some("sd"), "ea", 74)?;
        let orig_b_ino = ro_ino_at(&snap, Some(if case.same_dir { "sd" } else { "dd" }), "eb", 75)?;
        drop(m);

        for &pinned in &[false, true] {
            for &use_from in &[false, true] {
                let m = mount_test_ext4(snap.clone(), 76)?;
                let pin_pair = if pinned {
                    let na = resolve_path(&m.tables, None, path_a, rf).map_err(|_| "pin ea")?.inode;
                    let nb = resolve_path(&m.tables, None, path_b, rf).map_err(|_| "pin eb")?.inode;
                    let ga = OpenGuard::new_unchecked(Arc::clone(&na));
                    let gb = OpenGuard::new_unchecked(Arc::clone(&nb));
                    Some((na, nb, ga, gb))
                } else {
                    None
                };

                if use_from {
                    m.fault_cache.arm_write_fail_from(2);
                } else {
                    m.fault_cache.arm_write_fail_pair(2, 3);
                }
                let res = rename_at2(&m.tables, None, path_a, None, path_b, RenameFlags(RenameFlags::EXCHANGE));
                m.fault_cache.arm_write_fail_on(0);
                kassert!(matches!(res, Err(VfsError::Io)));

                let cur_a = resolve_path(&m.tables, None, path_a, rf).map_err(|_| "resolve post ea")?.inode;
                let cur_b = resolve_path(&m.tables, None, path_b, rf).map_err(|_| "resolve post eb")?.inode;
                let ga = OpenGuard::new_unchecked(Arc::clone(&cur_a));
                let gb = OpenGuard::new_unchecked(Arc::clone(&cur_b));
                let cur_a_is_dir = cur_a.inode_type() == InodeType::Dir;
                let cur_b_is_dir = cur_b.inode_type() == InodeType::Dir;
                unlink_at(&m.tables, None, path_a, cur_a_is_dir).map_err(|_| "unlink post ea")?;
                unlink_at(&m.tables, None, path_b, cur_b_is_dir).map_err(|_| "unlink post eb")?;
                kassert!(cur_a.is_detached());
                kassert!(cur_b.is_detached());
                kassert_eq!(cur_a.stat().map_err(|_| "stat detached ea")?.nlink, 0);
                kassert_eq!(cur_b.stat().map_err(|_| "stat detached eb")?.nlink, 0);
                kassert!(m.fs.lookup_live_by_lock_id(cur_a.lock_id()).is_none());
                kassert!(m.fs.lookup_live_by_lock_id(cur_b.lock_id()).is_none());
                drop((ga, gb, cur_a, cur_b, pin_pair));

                let (ra, _) = open_inode(
                    &m.tables,
                    None,
                    b"/reuse_b1",
                    OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                    0o644,
                    0,
                )
                .map_err(|_| "reuse b1")?;
                let (rb, _) = open_inode(
                    &m.tables,
                    None,
                    b"/reuse_b2",
                    OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                    0o644,
                    0,
                )
                .map_err(|_| "reuse b2")?;
                let r1 = ra.ino();
                let r2 = rb.ino();
                kassert!((r1 == orig_a_ino && r2 == orig_b_ino) || (r1 == orig_b_ino && r2 == orig_a_ino));
                drop((ra, rb, m));
                kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
            }
        }
    }

    // (C) Cold-cache directories carrying a pre-existing nlink_floor entering rename step-2
    //     (!same_parent & same_parent) and RENAME_EXCHANGE, plus nlink_ceiling same_parent & !same_parent.
    // (C1) !same_parent rename step-2 & RENAME_EXCHANGE with new_dir carrying nlink_floor = 2 (disk = 1)
    let m = mount_test_ext4(base_img.to_vec(), 77)?;
    let fd = mkdir_at(&m.tables, None, b"/floor_d", 0o755, 0).map_err(|_| "mkdir floor_d")?;
    let fd_orig_ino = fd.ino();
    drop(fd);
    mkdir_at(&m.tables, None, b"/other", 0o755, 0).map_err(|_| "mkdir other")?;
    mkdir_at(&m.tables, None, b"/other/mv_sub", 0o755, 0).map_err(|_| "mkdir mv_sub")?;
    m.fs.sync().map_err(|_| "sync C1")?;
    let snap_c1 = m.mem_dev.0.data.lock().clone();
    drop(m);

    // Arm Pair(2, 3) on rmdir("/floor_d") in cold cache -> /floor_d has disk links_count == 1, nlink_floor == 2.
    {
        let m = mount_test_ext4(snap_c1.clone(), 78)?;
        m.fault_cache.arm_write_fail_pair(2, 3);
        let res = unlink_at(&m.tables, None, b"/floor_d", true);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res, Err(VfsError::Io)));

        rename_at2(&m.tables, None, b"/other/mv_sub", None, b"/floor_d/mv_sub", RenameFlags::default())
            .map_err(|_| "rename into floor_d")?;
        let ro_fs = mount_ro_snapshot(&m, 79)?;
        let ro_fd = ro_fs.root_inode().lookup("floor_d").map_err(|_| "ro floor_d")?;
        kassert_eq!(ro_fd.stat().map_err(|_| "ro stat floor_d")?.nlink, 3);
        drop(ro_fs);

        let floor_d = resolve_path(&m.tables, None, b"/floor_d", rf).map_err(|_| "resolve floor_d")?.inode;
        kassert_eq!(floor_d.stat().map_err(|_| "stat floor_d")?.nlink, 3);
        unlink_at(&m.tables, None, b"/floor_d/mv_sub", true).map_err(|_| "rmdir mv_sub")?;
        kassert_eq!(floor_d.stat().map_err(|_| "stat floor_d empty")?.nlink, 2);
        let g = OpenGuard::new_unchecked(Arc::clone(&floor_d));
        unlink_at(&m.tables, None, b"/floor_d", true).map_err(|_| "rmdir floor_d")?;
        kassert!(floor_d.is_detached());
        kassert_eq!(floor_d.stat().map_err(|_| "stat floor_d det")?.nlink, 0);
        drop((g, floor_d));
        let reuse = mkdir_at(&m.tables, None, b"/reuse_fd", 0o755, 0).map_err(|_| "reuse fd")?.ino();
        kassert_eq!(reuse, fd_orig_ino);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (C2) same_parent rename step-2 with nlink_floor = 2 (disk = 1) & RENAME_EXCHANGE with new_dir nlink_floor = 2 (disk = 1)
    {
        let m = mount_test_ext4(snap_c1.clone(), 80)?;
        m.fault_cache.arm_write_fail_pair(2, 3);
        let res = unlink_at(&m.tables, None, b"/floor_d", true);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res, Err(VfsError::Io)));

        let (f1, _) = open_inode(
            &m.tables,
            None,
            b"/floor_d/f1",
            OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
            0o644,
            0,
        )
        .map_err(|_| "create floor_d/f1")?;
        drop(f1);
        rename_at2(&m.tables, None, b"/floor_d/f1", None, b"/floor_d/f2", RenameFlags::default())
            .map_err(|_| "same_parent file rename in floor_d")?;
        rename_at2(&m.tables, None, b"/other/mv_sub", None, b"/floor_d/f2", RenameFlags(RenameFlags::EXCHANGE))
            .map_err(|_| "exchange into floor_d")?;
        let ro_fs = mount_ro_snapshot(&m, 81)?;
        let ro_fd = ro_fs.root_inode().lookup("floor_d").map_err(|_| "ro floor_d ex")?;
        kassert_eq!(ro_fd.stat().map_err(|_| "ro stat floor_d ex")?.nlink, 3);
        drop(ro_fs);

        let floor_d = resolve_path(&m.tables, None, b"/floor_d", rf).map_err(|_| "resolve floor_d ex")?.inode;
        kassert_eq!(floor_d.stat().map_err(|_| "stat floor_d ex")?.nlink, 3);
        unlink_at(&m.tables, None, b"/floor_d/f2", true).map_err(|_| "rmdir floor_d/f2")?;
        let g = OpenGuard::new_unchecked(Arc::clone(&floor_d));
        unlink_at(&m.tables, None, b"/floor_d", true).map_err(|_| "rmdir floor_d ex")?;
        kassert!(floor_d.is_detached());
        drop((g, floor_d));
        let reuse = mkdir_at(&m.tables, None, b"/reuse_fd2", 0o755, 0).map_err(|_| "reuse fd2")?.ino();
        kassert_eq!(reuse, fd_orig_ino);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    // (C3) Cold-cache directory carrying pre-existing nlink_ceiling (disk = 4, ceiling = 3)
    //      entering same_parent rename step-2 and RENAME_EXCHANGE.
    let m = mount_test_ext4(base_img.to_vec(), 82)?;
    let cp = mkdir_at(&m.tables, None, b"/ceil_p", 0o755, 0).map_err(|_| "mkdir ceil_p")?;
    let cp_orig_ino = cp.ino();
    drop(cp);
    mkdir_at(&m.tables, None, b"/ceil_p/sub1", 0o755, 0).map_err(|_| "mkdir sub1")?;
    mkdir_at(&m.tables, None, b"/ceil_p/sub2", 0o755, 0).map_err(|_| "mkdir sub2")?;
    mkdir_at(&m.tables, None, b"/other", 0o755, 0).map_err(|_| "mkdir other C3")?;
    let (of, _) = open_inode(
        &m.tables,
        None,
        b"/other/f",
        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .map_err(|_| "create other/f")?;
    drop(of);
    m.fs.sync().map_err(|_| "sync C3")?;
    let snap_c3 = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/ceil_p/sub1", None, b"/other/f", RenameFlags(RenameFlags::EXCHANGE))
        .map_err(|_| "measure C3")?;
    let c3_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    drop(m);

    let mut tested_ceil_same_parent = false;
    for fail_at in 1..=c3_writes {
        let m = mount_test_ext4(snap_c3.clone(), 83)?;
        m.fault_cache.arm_write_fail_on(fail_at);
        let res = rename_at2(&m.tables, None, b"/ceil_p/sub1", None, b"/other/f", RenameFlags(RenameFlags::EXCHANGE));
        m.fault_cache.arm_write_fail_on(0);
        if res.is_err() {
            let ro_fs = mount_ro_snapshot(&m, 84)?;
            let ro_cp = ro_fs.root_inode().lookup("ceil_p").map_err(|_| "ro ceil_p")?;
            let ro_nlink = ro_cp.stat().map_err(|_| "ro stat ceil_p")?.nlink;
            let ro_swapped = ro_fs
                .root_inode()
                .lookup("other")
                .and_then(|o| o.lookup("f"))
                .is_ok_and(|f| f.inode_type() == InodeType::Dir);
            drop((ro_cp, ro_fs));
            if ro_swapped && ro_nlink == 4 {
                tested_ceil_same_parent = true;
                // Cold-cache same_parent dir rename on ceil_p (disk=4, nlink_ceiling=3).
                rename_at2(&m.tables, None, b"/ceil_p/sub2", None, b"/ceil_p/sub2_mv", RenameFlags::default())
                    .map_err(|_| "same_parent rename on ceil_p")?;
                let ro_fs2 = mount_ro_snapshot(&m, 85)?;
                let ro_cp2 = ro_fs2.root_inode().lookup("ceil_p").map_err(|_| "ro ceil_p 2")?;
                kassert_eq!(ro_cp2.stat().map_err(|_| "ro stat ceil_p 2")?.nlink, 3);
                drop(ro_fs2);

                // same_parent RENAME_EXCHANGE on ceil_p (sub2_mv <-> sub1 which is file 'f').
                rename_at2(
                    &m.tables,
                    None,
                    b"/ceil_p/sub2_mv",
                    None,
                    b"/ceil_p/sub1",
                    RenameFlags(RenameFlags::EXCHANGE),
                )
                .map_err(|_| "same_parent exchange on ceil_p")?;
                let ceil_p = resolve_path(&m.tables, None, b"/ceil_p", rf).map_err(|_| "resolve ceil_p")?.inode;
                kassert_eq!(ceil_p.stat().map_err(|_| "stat ceil_p after")?.nlink, 3);
                unlink_at(&m.tables, None, b"/ceil_p/sub1", true).map_err(|_| "rmdir moved sub2_mv")?;
                unlink_at(&m.tables, None, b"/ceil_p/sub2_mv", false).map_err(|_| "unlink swapped f")?;
                kassert_eq!(ceil_p.stat().map_err(|_| "stat ceil_p empty")?.nlink, 2);
                let g = OpenGuard::new_unchecked(Arc::clone(&ceil_p));
                unlink_at(&m.tables, None, b"/ceil_p", true).map_err(|_| "rmdir ceil_p")?;
                kassert!(ceil_p.is_detached());
                drop((g, ceil_p));
                let reuse = mkdir_at(&m.tables, None, b"/reuse_cp", 0o755, 0).map_err(|_| "reuse cp")?.ino();
                kassert_eq!(reuse, cp_orig_ino);
                drop(m);
                kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
                break;
            }
        }
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }
    kassert!(tested_ceil_same_parent);

    Ok(())
}

#[inline(never)]
fn sweep_r9_exchange_pair_and_notfound_recovery(base_img: &[u8], quota_before: usize) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use core::sync::atomic::Ordering;

    let rf = ResolveFlags { follow_final: false, empty_path: false, require_dir: false };

    let ro_ino_at = |img: &[u8], dir_name: Option<&str>, name: &str, minor: u32| -> Result<u64, &'static str> {
        let ro_dev = MemBlkDev::new(img.to_vec(), true);
        let ro_bc: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(SleepMutex::new(lumen_bcache::BlockCache::new(ro_dev, 32).map_err(|_| "ro bc")?));
        let ro_fs = ext4::Ext4FileSystem::mount(ro_bc, DevId::makedev(0, minor), true).map_err(|_| "ro mount")?;
        let dir = match dir_name {
            Some(d) => ro_fs.root_inode().lookup(d).map_err(|_| "ro lookup parent")?,
            None => ro_fs.root_inode(),
        };
        Ok(dir.lookup(name).map_err(|_| "ro lookup child")?.ino())
    };

    // (1) Exhaustive Pair(c1, c2) on RENAME_EXCHANGE across dir<->dir, dir<->sym, dir<->file
    //     (both cross-dir and same-dir, cold-cache and pinned), asserting zero entry loss,
    //     Gate 6 detach on cleanup, and exact inode reuse.
    #[derive(Clone, Copy)]
    enum Kind {
        File,
        Symlink,
        Dir,
    }
    struct PairExCase {
        same_dir: bool,
        a_kind: Kind,
        b_kind: Kind,
    }
    let cases = [
        PairExCase { same_dir: false, a_kind: Kind::Dir, b_kind: Kind::Dir },
        PairExCase { same_dir: true, a_kind: Kind::Dir, b_kind: Kind::Dir },
        PairExCase { same_dir: false, a_kind: Kind::Dir, b_kind: Kind::Symlink },
        PairExCase { same_dir: false, a_kind: Kind::Dir, b_kind: Kind::File },
        PairExCase { same_dir: true, a_kind: Kind::Dir, b_kind: Kind::File },
    ];
    for case in &cases {
        let m = mount_test_ext4(base_img.to_vec(), 86)?;
        mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd r9")?;
        if !case.same_dir {
            mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd r9")?;
        }
        let path_a: &[u8] = b"/sd/ea";
        let path_b: &[u8] = if case.same_dir { b"/sd/eb" } else { b"/dd/eb" };
        let make_entry = |path: &[u8], kind: Kind| -> Result<(), &'static str> {
            match kind {
                Kind::File => {
                    let (f, _) = open_inode(
                        &m.tables,
                        None,
                        path,
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "create ex r9")?;
                    kassert_eq!(f.write_at(0, b"R9_DATA").map_err(|_| "write ex r9")?, 7);
                }
                Kind::Symlink => {
                    symlink_at(&m.tables, b"/r9/sym/target", None, path).map_err(|_| "sym ex r9")?;
                }
                Kind::Dir => {
                    mkdir_at(&m.tables, None, path, 0o755, 0).map_err(|_| "mkdir ex r9")?;
                }
            }
            Ok(())
        };
        make_entry(path_a, case.a_kind)?;
        make_entry(path_b, case.b_kind)?;
        m.fs.sync().map_err(|_| "sync r9 ex")?;
        let snap = m.mem_dev.0.data.lock().clone();
        let orig_a_ino = ro_ino_at(&snap, Some("sd"), "ea", 87)?;
        let orig_b_ino = ro_ino_at(&snap, Some(if case.same_dir { "sd" } else { "dd" }), "eb", 88)?;
        m.fault_cache.arm_write_fail_on(0);
        rename_at2(&m.tables, None, path_a, None, path_b, RenameFlags(RenameFlags::EXCHANGE))
            .map_err(|_| "measure ex r9")?;
        let total_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
        kassert!(total_writes >= 12);
        drop(m);

        for &pinned in &[false, true] {
            for c1 in 1..=total_writes {
                for c2 in (c1 + 1)..=(c1 + 6) {
                    let m = mount_test_ext4(snap.clone(), 89)?;
                    let pin_pair = if pinned {
                        let na = resolve_path(&m.tables, None, path_a, rf).map_err(|_| "pin ea r9")?.inode;
                        let nb = resolve_path(&m.tables, None, path_b, rf).map_err(|_| "pin eb r9")?.inode;
                        let ga = OpenGuard::new_unchecked(Arc::clone(&na));
                        let gb = OpenGuard::new_unchecked(Arc::clone(&nb));
                        Some((na, nb, ga, gb))
                    } else {
                        None
                    };

                    m.fault_cache.arm_write_fail_pair(c1, c2);
                    let _ = rename_at2(&m.tables, None, path_a, None, path_b, RenameFlags(RenameFlags::EXCHANGE));
                    m.fault_cache.arm_write_fail_on(0);

                    let a_at_a = resolve_path(&m.tables, None, path_a, rf).is_ok_and(|r| r.inode.ino() == orig_a_ino);
                    let a_at_b = resolve_path(&m.tables, None, path_b, rf).is_ok_and(|r| r.inode.ino() == orig_a_ino);
                    let b_at_b = resolve_path(&m.tables, None, path_b, rf).is_ok_and(|r| r.inode.ino() == orig_b_ino);
                    let b_at_a = resolve_path(&m.tables, None, path_a, rf).is_ok_and(|r| r.inode.ino() == orig_b_ino);
                    kassert!((a_at_a && b_at_b) || (a_at_b && b_at_a));

                    let cur_a = resolve_path(&m.tables, None, path_a, rf).map_err(|_| "resolve post ea r9")?.inode;
                    let cur_b = resolve_path(&m.tables, None, path_b, rf).map_err(|_| "resolve post eb r9")?.inode;
                    kassert!(!cur_a.is_detached());
                    kassert!(!cur_b.is_detached());
                    let ga = OpenGuard::new_unchecked(Arc::clone(&cur_a));
                    let gb = OpenGuard::new_unchecked(Arc::clone(&cur_b));
                    let cur_a_is_dir = cur_a.inode_type() == InodeType::Dir;
                    let cur_b_is_dir = cur_b.inode_type() == InodeType::Dir;
                    kassert!(cur_a.stat().map_err(|_| "stat ea r9")?.nlink >= if cur_a_is_dir { 2 } else { 1 });
                    kassert!(cur_b.stat().map_err(|_| "stat eb r9")?.nlink >= if cur_b_is_dir { 2 } else { 1 });
                    unlink_at(&m.tables, None, path_a, cur_a_is_dir).map_err(|_| "unlink post ea r9")?;
                    unlink_at(&m.tables, None, path_b, cur_b_is_dir).map_err(|_| "unlink post eb r9")?;
                    kassert!(cur_a.is_detached());
                    kassert!(cur_b.is_detached());
                    kassert_eq!(cur_a.stat().map_err(|_| "stat det ea r9")?.nlink, 0);
                    kassert_eq!(cur_b.stat().map_err(|_| "stat det eb r9")?.nlink, 0);
                    kassert!(m.fs.lookup_live_by_lock_id(cur_a.lock_id()).is_none());
                    kassert!(m.fs.lookup_live_by_lock_id(cur_b.lock_id()).is_none());
                    drop((ga, gb, cur_a, cur_b, pin_pair));

                    let (ra, _) = open_inode(
                        &m.tables,
                        None,
                        b"/reuse_r9_a",
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "reuse r9 a")?;
                    let (rb, _) = open_inode(
                        &m.tables,
                        None,
                        b"/reuse_r9_b",
                        OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                        0o644,
                        0,
                    )
                    .map_err(|_| "reuse r9 b")?;
                    let r1 = ra.ino();
                    let r2 = rb.ino();
                    kassert!((r1 == orig_a_ino && r2 == orig_b_ino) || (r1 == orig_b_ino && r2 == orig_a_ino));
                    drop((ra, rb, m));
                    kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
                }
            }
        }
    }

    // (2) unlink_dir_with_recovery's Err(Ext4Error::NotFound) arm must not leak the parent's nlink:
    //     (2a) rmdir("/dd/sub") with each of its writes failing in turn
    let m = mount_test_ext4(base_img.to_vec(), 90)?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd r9")?;
    let dd_orig_ino = dd.ino();
    drop(dd);
    mkdir_at(&m.tables, None, b"/dd/sub", 0o755, 0).map_err(|_| "mkdir dd/sub r9")?;
    m.fs.sync().map_err(|_| "sync dd/sub r9")?;
    let snap_rmdir = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "measure rmdir dd/sub")?;
    let rmdir_sub_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(rmdir_sub_writes >= 10);
    drop(m);

    for &pinned_dd in &[false, true] {
        let mut saw_notfound_post_err = false;
        for fail_at in 1..=rmdir_sub_writes {
            let m = mount_test_ext4(snap_rmdir.clone(), 91)?;
            let pinned = if pinned_dd {
                let d = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "pin dd r9")?.inode;
                let g = OpenGuard::new_unchecked(Arc::clone(&d));
                Some((d, g))
            } else {
                None
            };
            m.fault_cache.arm_write_fail_on(fail_at);
            let res = unlink_at(&m.tables, None, b"/dd/sub", true);
            m.fault_cache.arm_write_fail_on(0);
            kassert!(res.is_ok() || matches!(res, Err(VfsError::Io)));

            if resolve_path(&m.tables, None, b"/dd/sub", rf).is_ok() {
                unlink_at(&m.tables, None, b"/dd/sub", true).map_err(|_| "cleanup dd/sub r9")?;
            } else if res.is_err() {
                saw_notfound_post_err = true;
            }
            let dd = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "resolve dd r9")?.inode;
            let g_dd = OpenGuard::new_unchecked(Arc::clone(&dd));
            kassert_eq!(dd.stat().map_err(|_| "stat dd before rmdir")?.nlink, 2);
            unlink_at(&m.tables, None, b"/dd", true).map_err(|_| "rmdir dd r9")?;
            kassert!(dd.is_detached());
            kassert_eq!(dd.stat().map_err(|_| "stat dd det r9")?.nlink, 0);
            kassert!(m.fs.lookup_live_by_lock_id(dd.lock_id()).is_none());
            drop((g_dd, dd, pinned));

            let reuse_ino = mkdir_at(&m.tables, None, b"/reuse_dd", 0o755, 0).map_err(|_| "reuse dd r9")?.ino();
            kassert_eq!(reuse_ino, dd_orig_ino);
            drop(m);
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        }
        kassert!(saw_notfound_post_err);
    }

    //     (2b) rename_at2("/sd/src", "/dd/dst") overwriting empty dir /dd/dst across fail_at=1..=total_writes
    let m = mount_test_ext4(base_img.to_vec(), 92)?;
    let dd = mkdir_at(&m.tables, None, b"/dd", 0o755, 0).map_err(|_| "mkdir dd ov r9")?;
    let dd_ov_orig_ino = dd.ino();
    drop(dd);
    mkdir_at(&m.tables, None, b"/sd", 0o755, 0).map_err(|_| "mkdir sd ov r9")?;
    mkdir_at(&m.tables, None, b"/sd/src", 0o755, 0).map_err(|_| "mkdir sd/src r9")?;
    mkdir_at(&m.tables, None, b"/dd/dst", 0o755, 0).map_err(|_| "mkdir dd/dst r9")?;
    m.fs.sync().map_err(|_| "sync ov r9")?;
    let snap_ov = m.mem_dev.0.data.lock().clone();
    m.fault_cache.arm_write_fail_on(0);
    rename_at2(&m.tables, None, b"/sd/src", None, b"/dd/dst", RenameFlags::default()).map_err(|_| "measure ov r9")?;
    let ov_writes = m.fault_cache.write_calls.load(Ordering::Acquire);
    kassert!(ov_writes >= 14);
    drop(m);

    for &pinned_dd in &[false, true] {
        for fail_at in 1..=ov_writes {
            let m = mount_test_ext4(snap_ov.clone(), 93)?;
            let pinned = if pinned_dd {
                let d = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "pin dd ov r9")?.inode;
                let g = OpenGuard::new_unchecked(Arc::clone(&d));
                Some((d, g))
            } else {
                None
            };
            m.fault_cache.arm_write_fail_on(fail_at);
            let res = rename_at2(&m.tables, None, b"/sd/src", None, b"/dd/dst", RenameFlags::default());
            m.fault_cache.arm_write_fail_on(0);
            kassert!(res.is_ok() || matches!(res, Err(VfsError::Io)));

            let dst_ok = resolve_path(&m.tables, None, b"/dd/dst", rf).is_ok();
            let src_ok = resolve_path(&m.tables, None, b"/sd/src", rf).is_ok();
            // A failed rename leaves its target in place; one that went
            // through leaves the source there: `/dd/dst` is never lost.
            kassert!(dst_ok);
            if dst_ok {
                unlink_at(&m.tables, None, b"/dd/dst", true).map_err(|_| "cleanup dd/dst r9")?;
            }
            if src_ok {
                unlink_at(&m.tables, None, b"/sd/src", true).map_err(|_| "cleanup sd/src r9")?;
            }
            let dd = resolve_path(&m.tables, None, b"/dd", rf).map_err(|_| "resolve dd ov r9")?.inode;
            let g_dd = OpenGuard::new_unchecked(Arc::clone(&dd));
            kassert_eq!(dd.stat().map_err(|_| "stat dd ov before rmdir")?.nlink, 2);
            unlink_at(&m.tables, None, b"/dd", true).map_err(|_| "rmdir dd ov r9")?;
            kassert!(dd.is_detached());
            kassert_eq!(dd.stat().map_err(|_| "stat dd ov det r9")?.nlink, 0);
            kassert!(m.fs.lookup_live_by_lock_id(dd.lock_id()).is_none());
            drop((g_dd, dd, pinned));

            let reuse_ino = mkdir_at(&m.tables, None, b"/reuse_dd_ov", 0o755, 0).map_err(|_| "reuse dd ov r9")?.ino();
            kassert_eq!(reuse_ino, dd_ov_orig_ino);
            drop(m);
            kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
        }
    }

    // (3) zero_inline_symlink_before_unlink under From(1), Pair(2, 3), and From(2):
    //     inline_60 and size_lo are zeroed atomically in one write_at so From(2) can never
    //     leave non-zero size pointing at NUL bytes, and From(1)/Pair(2,3) preserve target intact.
    let m = mount_test_ext4(base_img.to_vec(), 94)?;
    let sym = symlink_at(&m.tables, b"/sym/target/abc", None, b"/sym_z").map_err(|_| "create sym_z")?;
    let sym_orig_ino = sym.ino();
    drop(sym);
    m.fs.sync().map_err(|_| "sync sym_z")?;
    let snap_sym_z = m.mem_dev.0.data.lock().clone();
    drop(m);

    for &mode in &[0u8, 1, 2] {
        let m = mount_test_ext4(snap_sym_z.clone(), 95)?;
        match mode {
            0 => m.fault_cache.arm_write_fail_from(1),
            1 => m.fault_cache.arm_write_fail_pair(2, 3),
            _ => m.fault_cache.arm_write_fail_from(2),
        }
        let res = unlink_at(&m.tables, None, b"/sym_z", false);
        m.fault_cache.arm_write_fail_on(0);
        kassert!(matches!(res, Err(VfsError::Io)));

        let target = readlink_at(&m.tables, None, b"/sym_z").map_err(|_| "readlink sym_z")?;
        if mode < 2 {
            kassert_eq!(target, b"/sym/target/abc");
        } else {
            kassert!(target.is_empty() || target == b"/sym/target/abc");
            kassert!(!target.contains(&0));
        }

        let sym = resolve_path(&m.tables, None, b"/sym_z", rf).map_err(|_| "resolve sym_z")?.inode;
        let g = OpenGuard::new_unchecked(Arc::clone(&sym));
        unlink_at(&m.tables, None, b"/sym_z", false).map_err(|_| "retry unlink sym_z")?;
        kassert!(sym.is_detached());
        kassert_eq!(sym.stat().map_err(|_| "stat sym_z det")?.nlink, 0);
        kassert!(m.fs.lookup_live_by_lock_id(sym.lock_id()).is_none());
        drop((g, sym));

        let reuse_ino = symlink_at(&m.tables, b"/reused", None, b"/sym_z_reuse").map_err(|_| "reuse sym_z")?.ino();
        kassert_eq!(reuse_ino, sym_orig_ino);
        drop(m);
        kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
    }

    Ok(())
}

crate::ktest!(
    fn ext4_partial_phase2_io_error_recovery() {
        let base_img = build_minimal_ext4_image_blocks(256);
        let quota_before = page_cache::DETACHED_QUOTA.used_pages();

        sweep_rmdir_io_recovery(&base_img, quota_before)?;
        sweep_rename_dir_overwrite_io_recovery(&base_img, quota_before)?;
        sweep_inline_symlink_and_file_unlink_io_recovery(&base_img, quota_before)?;
        sweep_rename_step2_and_exchange_io_recovery(&base_img, quota_before)?;
        sweep_truncate_shrink_io_recovery(&base_img)?;
        sweep_cascading_eio_and_post_res_recovery(&base_img, quota_before)?;
        sweep_upward_drift_and_dir_floor_recovery(&base_img, quota_before)?;
        sweep_r9_exchange_pair_and_notfound_recovery(&base_img, quota_before)?;
        crate::kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);

        Ok(())
    }
);

crate::ktest!(
    fn vfs_append_writes_at_the_end_in_one_inode_lock_hold() {
        // `VfsInode::append` (#254, `O_APPEND`): where it wrote and how much,
        // at the end each time, on tmpfs and ext4; then appenders on every
        // CPU at once into one tmpfs file, every record whole and none lost.
        use crate::{kassert, kassert_eq};
        const RECORD: usize = 8;
        const PER_THREAD: usize = 256;

        let tfs = tmpfs::Tmpfs::new(DevId::makedev(0, 123), 64 * page_cache::PAGE_SIZE);
        let t = tfs.root_inode().create("a", 0o644).map_err(|_| "tmpfs create")?;
        kassert_eq!(t.write_at(0, b"xyz"), Ok(3));
        kassert_eq!(t.append(b"12"), Ok((3, 2)));
        kassert_eq!(t.append(b""), Ok((5, 0)));
        kassert_eq!(read_all_inode(&t).map_err(|_| "read tmpfs")?, b"xyz12");

        let m = mount_test_ext4(build_minimal_ext4_image_blocks(256), 141)?;
        let e = m.fs.root_inode().create("ap", 0o644).map_err(|_| "ext4 create")?;
        kassert_eq!(e.write_at(0, b"abc"), Ok(3));
        kassert_eq!(e.append(b"de"), Ok((3, 2)));
        kassert_eq!(e.append(b""), Ok((5, 0)));
        kassert_eq!(read_all_inode(&e).map_err(|_| "read ext4")?, b"abcde");
        drop(e);
        drop(m);

        let shared = tfs.root_inode().create("shared", 0o644).map_err(|_| "tmpfs create shared")?;
        let cpus: Vec<usize> =
            (0..crate::arch::MAX_CPUS).filter(|&c| crate::smp::online_lapic_id(c).is_some()).collect();
        let refused = core::sync::atomic::AtomicU32::new(0);
        let (shared_ref, refused_ref) = (&shared, &refused);
        let spawned = crate::sched::scope(|scope| {
            for (n, &cpu) in cpus.iter().enumerate() {
                scope.spawn_on(cpu, "append-ktest", move || {
                    for i in 0..PER_THREAD {
                        let rec = alloc::format!("{n:02}:{i:04}\n");
                        if shared_ref.append(rec.as_bytes()).map(|(_, n)| n) != Ok(RECORD) {
                            refused_ref.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        }
                    }
                })?;
            }
            Ok::<(), crate::sched::SpawnError>(())
        });
        spawned.map_err(|_| "spawn")?.map_err(|_| "spawn an appender")?;
        kassert_eq!(refused.load(core::sync::atomic::Ordering::Relaxed), 0);
        let all = read_all_inode(&shared).map_err(|_| "read shared")?;
        kassert_eq!(all.len(), cpus.len() * PER_THREAD * RECORD);
        let mut seen = alloc::vec![0u32; cpus.len() * PER_THREAD];
        for rec in all.chunks(RECORD) {
            let text = core::str::from_utf8(rec).map_err(|_| "a torn record")?;
            let (n, i) = text.trim_end().split_once(':').ok_or("a torn record")?;
            let n: usize = n.parse().map_err(|_| "a torn record")?;
            let i: usize = i.parse().map_err(|_| "a torn record")?;
            kassert!(n < cpus.len() && i < PER_THREAD && rec[RECORD - 1] == b'\n');
            seen[n * PER_THREAD + i] += 1;
        }
        kassert!(seen.iter().all(|&c| c == 1));
        Ok(())
    }
);
