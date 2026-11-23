//! In-memory POSIX filesystem (`tmpfs`) backed by unified `PageCache` frames
//! and bounded by an atomic per-mount memory quota (default 32 MiB). The
//! quota pays for the data pages, and also (#267) for each inode
//! ([`charge::INODE_COST`], and a directory's maps, [`charge::DIR_COST`]),
//! each directory entry ([`charge::entry_cost`]: its name and its share of
//! the maps) and each symlink's target, so files and names can't grow the
//! kernel heap without bound. Those costs are estimates from above, and
//! some of what a mount takes from the heap isn't charged: the root inode
//! and its maps' root nodes (one per mount), a file's page map, advisory
//! locks, and the VFS caches' entries (ARCHITECTURE section 9).

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, Escaped, FileSystem, Ino, InodeType, Metadata, RenameFlags, SetTime, Timespec,
    VfsError, VfsInode, mode_after_chown, next_detached_ino, next_lock_id,
};

use crate::sync::{SleepMutex, SleepMutexGuard};
use crate::time;

use super::page_cache::{self, CachePage, PAGE_SIZE, PageCache, QuotaCounter};
use charge::{Children, FIRST_CHILD_CURSOR, Growth, InodeCharge};
#[cfg(feature = "ktest")]
use charge::{DIR_COST, INODE_COST, entry_cost, heap_block};

struct TmpfsShared {
    dev: DevId,
    quota: Arc<QuotaCounter>,
    next_ino: AtomicU64,
}

/// In-memory `tmpfs` filesystem (`0:2` on `/tmp`, or `0:1` on diskless root `/`).
pub struct Tmpfs {
    shared: Arc<TmpfsShared>,
    rename_lock: SleepMutex<()>,
    root: Arc<TmpfsInode>,
}

impl Tmpfs {
    /// A mount whose pages, inodes, names and symlink targets share
    /// `quota_bytes`.
    pub fn new(dev: DevId, quota_bytes: usize) -> Arc<Self> {
        let shared = Arc::new(TmpfsShared {
            dev,
            quota: Arc::new(QuotaCounter::for_mount("tmpfs", dev, quota_bytes)),
            next_ino: AtomicU64::new(2),
        });
        let now = time::realtime_timespec();
        let root = Arc::new_cyclic(|weak_self| TmpfsInode {
            lock_id: next_lock_id(),
            shared: Arc::clone(&shared),
            self_weak: Weak::clone(weak_self),
            ino: AtomicU64::new(1),
            inode_type: InodeType::Dir,
            _charged: InodeCharge::uncharged_root(&shared.quota),
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            meta: SleepMutex::new(TmpfsMeta {
                mode: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                size: 0,
                atime: now,
                mtime: now,
                ctime: now,
                node: Node::Dir(Dir {
                    children: Children::new(Arc::clone(&shared.quota)),
                    parent: Weak::clone(weak_self),
                }),
                locks: AdvisoryLocks::new(),
            }),
            pages: SleepMutex::new(PageCache::new_tmpfs(Arc::clone(&shared.quota))),
        });
        Arc::new(Self { shared, rename_lock: SleepMutex::new(()), root })
    }

    /// The bytes of the quota in use: pages, inodes, names and targets.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub fn used_bytes(&self) -> usize {
        self.shared.quota.used_bytes()
    }
}

/// Room for a ktest's few small files' inodes and names on top of a whole
/// number of pages of data: under a page, so such a mount still runs out
/// of data pages exactly where a quota of only those pages would, as long
/// as the files' charges ([`INODE_COST`] and [`entry_cost`] each, a
/// directory's [`DIR_COST`] too) fit in it: six files named with up to 16
/// bytes at most (656 bytes each as of #267), or one directory and three
/// such files (both checked below at compile time). Past that the files'
/// charges take the data's room: creating them still works, but the mount
/// runs out of data pages early.
#[cfg(feature = "ktest")]
pub(crate) const KTEST_METADATA_ROOM: usize = PAGE_SIZE - 1;

#[cfg(feature = "ktest")]
const _: () = {
    let file = INODE_COST + entry_cost(16);
    assert!(6 * file <= KTEST_METADATA_ROOM);
    assert!(INODE_COST + DIR_COST + entry_cost(16) + 3 * file <= KTEST_METADATA_ROOM);
};

impl FileSystem for Tmpfs {
    fn dev_id(&self) -> DevId {
        self.shared.dev
    }

    fn name(&self) -> &'static str {
        "tmpfs"
    }

    fn root_inode(&self) -> Arc<dyn VfsInode> {
        Arc::clone(&self.root) as Arc<dyn VfsInode>
    }

    fn sync(&self) -> Result<(), VfsError> {
        Ok(())
    }

    fn read_only(&self) -> bool {
        false
    }

    fn with_rename_lock(&self, f: &mut dyn FnMut() -> Result<(), VfsError>) -> Result<(), VfsError> {
        let _guard = self.rename_lock.lock();
        f()
    }
}

struct TmpfsMeta {
    mode: u16,
    nlink: u32,
    uid: u32,
    gid: u32,
    size: u64,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
    node: Node,
    locks: AdvisoryLocks,
}

impl TmpfsMeta {
    /// A directory's own part, or `NotDir`.
    fn dir(&self) -> Result<&Dir, VfsError> {
        match &self.node {
            Node::Dir(dir) => Ok(dir),
            _ => Err(VfsError::NotDir),
        }
    }

    fn dir_mut(&mut self) -> Result<&mut Dir, VfsError> {
        match &mut self.node {
            Node::Dir(dir) => Ok(dir),
            _ => Err(VfsError::NotDir),
        }
    }

    /// A directory's own part, for an inode already known to be one (by its
    /// `inode_type`, fixed with its `node`): where a change has been made,
    /// so that failing would leave it half done.
    fn known_dir_mut(&mut self) -> &mut Dir {
        self.dir_mut().expect("tmpfs: a directory's node is a Dir")
    }

    /// A directory's parent (the root's is itself), if it is still there.
    fn parent(&self) -> Option<Arc<TmpfsInode>> {
        self.dir().ok().and_then(|dir| dir.parent.upgrade())
    }
}

/// What only one kind of inode has. An inode's kind is fixed when it is
/// made, its `inode_type` taken from it ([`Node::inode_type`]).
enum Node {
    /// Its data is in the inode's `pages`.
    File,
    Dir(Dir),
    /// The target, as given (charged to the quota with the inode).
    Symlink(Box<[u8]>),
}

impl Node {
    fn inode_type(&self) -> InodeType {
        match self {
            Node::File => InodeType::File,
            Node::Dir(_) => InodeType::Dir,
            Node::Symlink(_) => InodeType::Symlink,
        }
    }
}

struct Dir {
    children: Children,
    /// The directory holding this one; the root's own.
    parent: Weak<TmpfsInode>,
}

/// What a mount's inodes, names and symlink targets cost its quota (#267),
/// and the charges that carry those costs: only this module reserves or
/// gives back a charge, so every byte reserved has one owner and one
/// release. The costs are estimates from above of what each takes from the
/// kernel heap (`tmpfs_quota_charges_cover_what_the_heap_gives` measures
/// them), from std's `BTreeMap` layout and `lumen_heap`'s blocks.
mod charge {
    use alloc::collections::BTreeMap;
    use alloc::sync::Arc;
    use alloc::vec::Vec;

    use lumen_vfs::{Escaped, VfsError};

    use super::{Node, QuotaCounter, TmpfsInode};

    /// What each inode but the root costs, besides its pages and what
    /// [`inode_cost`] adds for its kind: the inode as allocated, the struct
    /// and an `Arc`'s two counts in one heap block.
    pub(super) const INODE_COST: usize = heap_block(size_of::<TmpfsInode>() + 2 * size_of::<usize>());

    /// What a directory costs besides [`INODE_COST`]: the root node of each
    /// of its maps, which may hold a single entry.
    pub(super) const DIR_COST: usize = <BySeq as MapNode>::NODE + <ByName as MapNode>::NODE;

    /// What a directory entry named with `name_len` bytes costs: the name's
    /// heap block, which [`Children`] keeps twice, and its share of the
    /// maps' nodes past their roots (which are [`DIR_COST`]), each of which
    /// holds at least [`BTREE_MIN_LEN`] entries.
    pub(super) const fn entry_cost(name_len: usize) -> usize {
        2 * heap_block(name_len) + DIR_COST.div_ceil(BTREE_MIN_LEN)
    }

    /// What an inode of `node`'s kind costs: [`INODE_COST`], and a
    /// directory's [`DIR_COST`] or a symlink's target's heap block.
    fn inode_cost(node: &Node) -> usize {
        INODE_COST
            + match node {
                Node::File => 0,
                Node::Dir(_) => DIR_COST,
                Node::Symlink(target) => heap_block(target.len()),
            }
    }

    /// The heap block holding `bytes`: `lumen_heap` rounds every block up
    /// to its granule and keeps no header.
    pub(super) const fn heap_block(bytes: usize) -> usize {
        bytes.next_multiple_of(lumen_heap::GRANULE)
    }

    /// std's `BTreeMap` branching factor: a node holds at most `2 * B - 1`
    /// entries, an internal one `2 * B` edges too, and every node but the
    /// root at least `B - 1` entries.
    const BTREE_B: usize = 6;
    const BTREE_MIN_LEN: usize = BTREE_B - 1;

    /// A bound on one node of a map, from its key and value types.
    trait MapNode {
        const NODE: usize;
    }

    impl<K, V> MapNode for BTreeMap<K, V> {
        /// The larger, internal kind of node as std lays it out: a parent
        /// pointer, its index there and the length (two `u16`s), the keys
        /// and values, padding to a pointer's alignment, and the edges. The
        /// padding is at most that alignment less one only while keys and
        /// values are whole words (no padding between the arrays), which
        /// this checks at compile time.
        const NODE: usize = {
            let word = align_of::<usize>();
            assert!(size_of::<K>().is_multiple_of(word) && align_of::<K>() <= word);
            assert!(size_of::<V>().is_multiple_of(word) && align_of::<V>() <= word);
            heap_block(
                size_of::<usize>()
                    + 2 * size_of::<u16>()
                    + (2 * BTREE_B - 1) * (size_of::<K>() + size_of::<V>())
                    + (word - 1)
                    + 2 * BTREE_B * size_of::<usize>(),
            )
        };
    }

    /// Bytes reserved from a mount's quota, given back when dropped.
    struct Reserved {
        quota: Arc<QuotaCounter>,
        bytes: usize,
    }

    impl Reserved {
        /// Reserve `bytes`, or `NoSpc` past the quota.
        fn new(quota: &Arc<QuotaCounter>, bytes: usize) -> Result<Self, VfsError> {
            if quota.try_reserve_bytes(bytes) {
                Ok(Self { quota: Arc::clone(quota), bytes })
            } else {
                Err(VfsError::NoSpc)
            }
        }
    }

    impl Drop for Reserved {
        /// Takes no sleeping lock (an over-release's `error!` takes the
        /// log's leaf lock).
        fn drop(&mut self) {
            self.quota.release_bytes(self.bytes);
        }
    }

    /// An inode's charge ([`inode_cost`]), held by the inode and given back
    /// when it is dropped: an unlinked inode keeps it until its last
    /// reference goes (an open file, a mapping).
    pub(super) struct InodeCharge {
        _reserved: Reserved,
    }

    impl InodeCharge {
        /// Reserve what an inode of `node`'s kind costs, or `NoSpc`.
        pub(super) fn reserve(quota: &Arc<QuotaCounter>, node: &Node) -> Result<Self, VfsError> {
            Reserved::new(quota, inode_cost(node)).map(|reserved| Self { _reserved: reserved })
        }

        /// The root's, which is free (one per mount): only [`super::Tmpfs::new`]
        /// may make one, as it pays for nothing.
        pub(super) fn uncharged_root(quota: &Arc<QuotaCounter>) -> Self {
            Self { _reserved: Reserved { quota: Arc::clone(quota), bytes: 0 } }
        }
    }

    /// A directory entry's charge ([`entry_cost`]) and the name it pays
    /// for: reserved for a new name ([`Children::reserve_new`]) or taken
    /// out of a directory with its entry ([`Children::take`]), spent by
    /// [`Children::add`], which files the name under it, and given back if
    /// dropped instead.
    pub(super) struct EntryCharge {
        reserved: Reserved,
        name: Vec<u8>,
    }

    impl EntryCharge {
        /// Reserve `name`'s charge, or `NoSpc`.
        fn for_name(quota: &Arc<QuotaCounter>, name: &[u8]) -> Result<Self, VfsError> {
            let reserved = Reserved::new(quota, entry_cost(name.len()))?;
            Ok(Self { reserved, name: name.to_vec() })
        }

        /// This charge, with `growth`, now paying for the name `growth` was
        /// reserved for instead: a rename's. What the old name's charge
        /// holds past the new name's cost is given back.
        pub(super) fn renamed(self, growth: Growth) -> Self {
            let Self { mut reserved, name: old_name } = self;
            let Growth { reserved: mut more, old_len, name } = growth;
            debug_assert_eq!(
                old_name.len(),
                old_len,
                "tmpfs: a rename's growth was reserved for another name's length"
            );
            debug_assert!(Arc::ptr_eq(&reserved.quota, &more.quota), "tmpfs: a rename across quotas");
            reserved.bytes += core::mem::take(&mut more.bytes);
            let cost = entry_cost(name.len());
            debug_assert!(reserved.bytes >= cost, "tmpfs: renaming to {:?} reserved too little", Escaped(&name));
            reserved.quota.release_bytes(reserved.bytes.saturating_sub(cost));
            reserved.bytes = cost;
            Self { reserved, name }
        }
    }

    /// What a rename to a new name costs on top of the old name's charge,
    /// which moves to it, and that new name: reserved before anything
    /// changes, so the count never passes the limit and a name no costlier
    /// than the old one needs nothing.
    pub(super) struct Growth {
        reserved: Reserved,
        /// The length of the old name it was sized against.
        old_len: usize,
        /// The new name.
        name: Vec<u8>,
    }

    impl Growth {
        /// Reserve what `new_name` costs more than `old_name`, or `NoSpc`.
        pub(super) fn reserve(quota: &Arc<QuotaCounter>, old_name: &[u8], new_name: &[u8]) -> Result<Self, VfsError> {
            let more = entry_cost(new_name.len()).saturating_sub(entry_cost(old_name.len()));
            let reserved = Reserved::new(quota, more)?;
            Ok(Self { reserved, old_len: old_name.len(), name: new_name.to_vec() })
        }
    }

    type BySeq = BTreeMap<u64, (Vec<u8>, Arc<TmpfsInode>)>;
    type ByName = BTreeMap<Vec<u8>, u64>;

    /// `readdir`'s cursor for the first entry after `.` (0) and `..` (1).
    pub(super) const FIRST_CHILD_CURSOR: u64 = 2;

    /// A directory's entries, each under the sequence number it was added
    /// with: `readdir`'s cursor. An entry keeps its number until it is
    /// removed (replacing what a name points to, a `rename` over it or a
    /// `RENAME_EXCHANGE`, keeps the name's number), and a new name takes
    /// the next number, never one used before; so a cursor between two
    /// calls still starts where it did, whatever was added or removed
    /// meanwhile.
    ///
    /// An entry's charge, `entry_cost(name.len())`, is held implicitly
    /// from [`Children::add`] until it is removed, taken out, or the
    /// directory dropped: `add` empties the [`EntryCharge`], and `take` and
    /// the directory's drop work the charge out again from the name. So an
    /// entry's cost must depend on its name's length alone.
    pub(super) struct Children {
        /// The mount's quota, which the entries' charges are given back to.
        quota: Arc<QuotaCounter>,
        /// The entries by number: what `readdir` walks.
        by_seq: BySeq,
        /// Each name's number, for lookups; always the same names as
        /// `by_seq`.
        by_name: ByName,
        /// The next name's number, from [`FIRST_CHILD_CURSOR`].
        next_seq: u64,
    }

    impl Children {
        pub(super) fn new(quota: Arc<QuotaCounter>) -> Self {
            Self { quota, by_seq: BTreeMap::new(), by_name: BTreeMap::new(), next_seq: FIRST_CHILD_CURSOR }
        }

        pub(super) fn get(&self, name: &[u8]) -> Option<&Arc<TmpfsInode>> {
            let seq = self.by_name.get(name)?;
            let entry = self.by_seq.get(seq);
            debug_assert!(
                entry.is_some_and(|(n, _)| n == name),
                "tmpfs: {:?}'s number {seq} lists another entry",
                Escaped(name)
            );
            entry.map(|(_, child)| child)
        }

        /// Reserve the charge for a new entry `name`: `Exist` if the name is
        /// taken (whether or not there is room), else `NoSpc` past the
        /// quota.
        pub(super) fn reserve_new(&self, name: &[u8]) -> Result<EntryCharge, VfsError> {
            if self.by_name.contains_key(name) {
                return Err(VfsError::Exist);
            }
            EntryCharge::for_name(&self.quota, name)
        }

        pub(super) fn is_empty(&self) -> bool {
            debug_assert_eq!(self.by_seq.len(), self.by_name.len());
            self.by_seq.is_empty()
        }

        pub(super) fn values(&self) -> impl Iterator<Item = &Arc<TmpfsInode>> {
            self.by_seq.values().map(|(_, child)| child)
        }

        /// Add `charge`'s name for `child`, under the next number: the
        /// directory now holds the charge.
        pub(super) fn add(&mut self, child: Arc<TmpfsInode>, charge: EntryCharge) {
            let EntryCharge { mut reserved, name } = charge;
            debug_assert!(!self.by_name.contains_key(&name), "tmpfs: {:?} added twice", Escaped(&name));
            debug_assert!(
                Arc::ptr_eq(&reserved.quota, &self.quota) && reserved.bytes == entry_cost(name.len()),
                "tmpfs: {:?} charged {} bytes, or to another quota",
                Escaped(&name),
                reserved.bytes
            );
            // From here this directory gives it back (`take`, its drop).
            reserved.bytes = 0;
            let seq = self.next_seq;
            self.next_seq += 1;
            self.by_name.insert(name.clone(), seq);
            self.by_seq.insert(seq, (name, child));
        }

        /// Point the existing `name` at `child`, keeping its number (and
        /// its charge).
        pub(super) fn replace(&mut self, name: &[u8], child: Arc<TmpfsInode>) {
            let slot = self.by_name.get(name).and_then(|seq| self.by_seq.get_mut(seq));
            debug_assert!(slot.is_some(), "tmpfs: replacing {:?}, which has no entry", Escaped(name));
            if let Some(slot) = slot {
                slot.1 = child;
            }
        }

        /// Take `name`'s entry out, with its charge (which a rename moves
        /// to the new name, and which is given back if dropped).
        pub(super) fn take(&mut self, name: &[u8]) -> Option<(Arc<TmpfsInode>, EntryCharge)> {
            let seq = *self.by_name.get(name)?;
            // Maps out of step (a bug) take nothing out: the name keeps its
            // entry and its charge.
            let listed = self.by_seq.get(&seq).is_some_and(|(n, _)| n == name);
            debug_assert!(listed, "tmpfs: {:?}'s number {seq} lists another entry, or none", Escaped(name));
            if !listed {
                return None;
            }
            let (name, _) = self.by_name.remove_entry(name)?;
            let (_, child) = self.by_seq.remove(&seq)?;
            // The name's charge comes out with it, sized and named from the
            // one key.
            let reserved = Reserved { quota: Arc::clone(&self.quota), bytes: entry_cost(name.len()) };
            Some((child, EntryCharge { reserved, name }))
        }

        /// Remove `name`, giving its charge back.
        pub(super) fn remove(&mut self, name: &[u8]) -> Option<Arc<TmpfsInode>> {
            self.take(name).map(|(child, _charge)| child)
        }

        /// The entries numbered `from` or more, in number order (the order
        /// they were added in): `(number, name, inode)`.
        pub(super) fn listed_from(&self, from: u64) -> impl Iterator<Item = (u64, &[u8], &Arc<TmpfsInode>)> {
            self.by_seq.range(from..).map(|(&seq, (name, child))| (seq, name.as_slice(), child))
        }
    }

    impl Drop for Children {
        /// A directory dropped with entries (only ever with its whole
        /// mount: `rmdir` and a rename over a directory take only empty
        /// ones) gives their charges back. Takes no sleeping lock (an
        /// over-release's `error!` takes the log's leaf lock).
        fn drop(&mut self) {
            let bytes = self.by_name.keys().map(|name| entry_cost(name.len())).sum();
            self.quota.release_bytes(bytes);
        }
    }
}

/// Inode belonging to a [`Tmpfs`] mount.
///
/// `Drop` never acquires any sleeping lock.
pub struct TmpfsInode {
    lock_id: u64,
    shared: Arc<TmpfsShared>,
    self_weak: Weak<TmpfsInode>,
    ino: AtomicU64,
    /// The kind of [`TmpfsMeta::node`], readable without its lock.
    inode_type: InodeType,
    /// What this inode holds of the quota besides its pages and its
    /// names (nothing for the root), given back when it is dropped.
    _charged: InodeCharge,
    open_count: AtomicU32,
    detached: AtomicBool,
    meta: SleepMutex<TmpfsMeta>,
    pages: SleepMutex<PageCache>,
}

impl TmpfsInode {
    /// `write_at` at `offset`, or at the end (`append`, `None`): the size is
    /// read under the same hold of `meta` (Lock 4) the write is made in.
    /// Returns where the write began and how much was written.
    fn write_locked(&self, offset: Option<u64>, buf: &[u8]) -> Result<(u64, usize), VfsError> {
        if self.inode_type == InodeType::Dir {
            return Err(VfsError::IsDir);
        }
        if self.inode_type != InodeType::File {
            return Err(VfsError::Inval);
        }
        let mut meta = self.meta.lock();
        let offset = offset.unwrap_or(meta.size);
        if buf.is_empty() {
            return Ok((offset, 0));
        }
        let mut pages = self.pages.lock();
        let old_size = meta.size;
        let _ = offset.checked_add(buf.len() as u64).ok_or(VfsError::FBig)?;
        if offset > old_size && !old_size.is_multiple_of(PAGE_SIZE as u64) {
            let tail_page = old_size / PAGE_SIZE as u64;
            let tail_off = (old_size % PAGE_SIZE as u64) as usize;
            if let Some(page) = pages.get(tail_page) {
                page.zero_range(tail_off, PAGE_SIZE);
            }
        }
        let mut done = 0usize;
        while done < buf.len() {
            let cur_off = offset + done as u64;
            let page_idx = cur_off / PAGE_SIZE as u64;
            let in_page = (cur_off % PAGE_SIZE as u64) as usize;
            let chunk = (PAGE_SIZE - in_page).min(buf.len() - done);
            match page_or_new(&mut pages, page_idx) {
                Ok(page) => page.write_bytes(in_page, &buf[done..done + chunk]),
                Err(err) => {
                    if done > 0 {
                        break;
                    }
                    return Err(err);
                }
            }
            done += chunk;
        }
        let written_end = offset + done as u64;
        let now = time::realtime_timespec();
        if written_end > meta.size {
            meta.size = written_end;
        }
        meta.mtime = now;
        meta.ctime = now;
        Ok((offset, done))
    }

    /// A new inode of `node`'s kind, charged to the quota ([`InodeCharge`],
    /// `NoSpc` past it) until it is dropped.
    fn new_child(shared: &Arc<TmpfsShared>, node: Node, mode: u16, nlink: u32) -> Result<Arc<Self>, VfsError> {
        let charged = InodeCharge::reserve(&shared.quota, &node)?;
        let size = match &node {
            Node::Symlink(target) => target.len(),
            Node::File | Node::Dir(_) => 0,
        };
        let now = time::realtime_timespec();
        let ino = shared.next_ino.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new_cyclic(|weak_self| Self {
            lock_id: next_lock_id(),
            shared: Arc::clone(shared),
            self_weak: Weak::clone(weak_self),
            ino: AtomicU64::new(ino),
            inode_type: node.inode_type(),
            _charged: charged,
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            meta: SleepMutex::new(TmpfsMeta {
                mode: mode & 0o7777,
                nlink,
                uid: 0,
                gid: 0,
                size: size as u64,
                atime: now,
                mtime: now,
                ctime: now,
                node,
                locks: AdvisoryLocks::new(),
            }),
            pages: SleepMutex::new(PageCache::new_tmpfs(Arc::clone(&shared.quota))),
        }))
    }

    fn detach_if_open(target: &Arc<TmpfsInode>, meta: &mut TmpfsMeta) {
        meta.nlink = 0;
        if target.open_count() > 0 && !target.is_detached() {
            target.detached.store(true, Ordering::Release);
            target.ino.store(next_detached_ino(), Ordering::Release);
        }
    }

    fn self_arc(&self) -> Option<Arc<TmpfsInode>> {
        self.self_weak.upgrade()
    }

    fn validate_name(name: &[u8]) -> Result<(), VfsError> {
        if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
            return Err(VfsError::Inval);
        }
        if name.len() > lumen_vfs::NAME_MAX {
            return Err(VfsError::NameTooLong);
        }
        Ok(())
    }

    /// This file's page-cache lock (Lock 5), for
    /// `page_cache::MappedPage::copy_to_frame`.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed MAP_PRIVATE CoW (#255)
    pub(super) fn page_cache_lock(&self) -> &SleepMutex<PageCache> {
        &self.pages
    }

    /// This file's page `index`, a zero page allocated for a hole, and its
    /// size read with it: `tmpfs`'s side of [`page_cache::mmap_page`] (its
    /// contract). The hole's page
    /// counts against the mount's quota, as a write's would.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed mmap (#255)
    pub(super) fn mmap_page(&self, index: u64) -> Result<(Arc<CachePage>, u64), VfsError> {
        if self.inode_type != InodeType::File {
            return Err(VfsError::NoDev);
        }
        let meta = self.meta.lock();
        let file_size = meta.size;
        page_cache::mmap_page_in_file(index, file_size)?;
        let mut pages = self.pages.lock();
        let page = page_or_new(&mut pages, index)?;
        page.touch();
        Ok((page, file_size))
    }
}

/// The cached page `index`, or a new zero page inserted there (`NoMem`, or
/// `NoSpc` past the quota): the one way `write_at` and `mmap_page` get a
/// page. (`read_at` reads a hole as zeros without one.) The caller holds
/// the inode's Lock 4 and the `pages` lock (Lock 5).
fn page_or_new(pages: &mut PageCache, index: u64) -> Result<Arc<CachePage>, VfsError> {
    if let Some(page) = pages.get(index) {
        return Ok(Arc::clone(page));
    }
    let page = Arc::new(CachePage::new_zeroed().ok_or(VfsError::NoMem)?);
    pages.insert_new(index, Arc::clone(&page))?;
    Ok(page)
}

struct LockedTmpfs<'a> {
    inode: &'a Arc<TmpfsInode>,
    guard: SleepMutexGuard<'a, TmpfsMeta>,
}

fn lock_tmpfs_ordered<'a>(inodes: &[&'a Arc<TmpfsInode>]) -> Vec<LockedTmpfs<'a>> {
    let mut unique: Vec<&'a Arc<TmpfsInode>> = Vec::with_capacity(inodes.len());
    for &ino in inodes {
        if !unique.iter().any(|x| x.lock_id == ino.lock_id) {
            unique.push(ino);
        }
    }
    unique.sort_by_key(|x| (x.inode_type != InodeType::Dir, x.lock_id));
    unique.into_iter().map(|inode| LockedTmpfs { inode, guard: inode.meta.lock() }).collect()
}

fn tmpfs_guard_mut<'a, 'b>(guards: &'b mut [LockedTmpfs<'a>], target: &Arc<TmpfsInode>) -> &'b mut TmpfsMeta {
    &mut guards.iter_mut().find(|g| g.inode.lock_id == target.lock_id).expect("locked tmpfs inode").guard
}

/// Resolve an `Arc<dyn VfsInode>` to its concrete `Arc<TmpfsInode>` by walking
/// up to root and scanning the directory tree.
fn find_tmpfs_dir_arc(known_dir: &TmpfsInode, target_lock_id: u64, target_ino: Ino) -> Option<Arc<TmpfsInode>> {
    let mut cur = known_dir.self_arc()?;
    if cur.lock_id == target_lock_id {
        return Some(cur);
    }
    loop {
        if cur.lock_id == target_lock_id {
            return Some(cur);
        }
        let next = cur.meta.lock().parent();
        match next {
            Some(p) if p.lock_id != cur.lock_id => cur = p,
            _ => break,
        }
    }
    // Depth-first search from the root (`cur`) for `target_lock_id`.
    let mut stack = alloc::vec![cur];
    while let Some(node) = stack.pop() {
        if node.lock_id == target_lock_id {
            return Some(node);
        }
        let m = node.meta.lock();
        let Ok(dir) = m.dir() else { continue };
        for child in dir.children.values() {
            if child.inode_type == InodeType::Dir {
                if child.lock_id == target_lock_id || child.ino() == target_ino {
                    return Some(Arc::clone(child));
                }
                stack.push(Arc::clone(child));
            }
        }
    }
    None
}

impl VfsInode for TmpfsInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }

    fn dev_id(&self) -> DevId {
        self.shared.dev
    }

    fn ino(&self) -> Ino {
        self.ino.load(Ordering::Acquire)
    }

    fn inode_type(&self) -> InodeType {
        self.inode_type
    }

    fn open_count(&self) -> u32 {
        self.open_count.load(Ordering::Acquire)
    }

    fn inc_open_count(&self) {
        self.open_count.fetch_add(1, Ordering::AcqRel);
    }

    fn dec_open_count(&self) -> u32 {
        self.open_count.fetch_sub(1, Ordering::AcqRel) - 1
    }

    fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    fn stat(&self) -> Result<Metadata, VfsError> {
        let meta = self.meta.lock();
        let blocks = if self.inode_type == InodeType::File {
            let pages = self.pages.lock();
            (pages.len() as u64) * 8
        } else {
            0
        };
        Ok(Metadata {
            dev: self.shared.dev,
            ino: self.ino(),
            inode_type: self.inode_type,
            mode: meta.mode,
            nlink: meta.nlink,
            uid: meta.uid,
            gid: meta.gid,
            rdev: DevId::default(),
            size: meta.size,
            blksize: PAGE_SIZE as u32,
            blocks,
            atime: meta.atime,
            mtime: meta.mtime,
            ctime: meta.ctime,
        })
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        let meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        if name == b"." {
            return self.self_arc().map(|a| a as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt);
        }
        if name == b".." {
            let p = meta.parent().ok_or(VfsError::NoEnt)?;
            return Ok(p as Arc<dyn VfsInode>);
        }
        meta.dir()?.children.get(name).map(|c| Arc::clone(c) as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt)
    }

    fn create(&self, name: &[u8], mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let children = &mut meta.dir_mut()?.children;
        let entry = children.reserve_new(name)?;
        let child = Self::new_child(&self.shared, Node::File, mode, 1)?;
        children.add(Arc::clone(&child), entry);
        let now = time::realtime_timespec();
        meta.mtime = now;
        meta.ctime = now;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn mkdir(&self, name: &[u8], mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let children = &mut meta.dir_mut()?.children;
        let entry = children.reserve_new(name)?;
        let dir = Dir { children: Children::new(Arc::clone(&self.shared.quota)), parent: Arc::downgrade(&self_arc) };
        let child = Self::new_child(&self.shared, Node::Dir(dir), mode, 2)?;
        children.add(Arc::clone(&child), entry);
        meta.nlink = meta.nlink.saturating_add(1);
        let now = time::realtime_timespec();
        meta.mtime = now;
        meta.ctime = now;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn unlink(&self, name: &[u8]) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == b"." || name == b".." {
            return Err(VfsError::IsDir);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let child_arc = {
            let meta = self_arc.meta.lock();
            if self_arc.is_detached() || meta.nlink == 0 {
                return Err(VfsError::NoEnt);
            }
            let c = meta.dir()?.children.get(name).ok_or(VfsError::NoEnt)?;
            if c.inode_type == InodeType::Dir {
                return Err(VfsError::IsDir);
            }
            Arc::clone(c)
        };

        let mut guards = lock_tmpfs_ordered(&[&self_arc, &child_arc]);
        let now = time::realtime_timespec();
        {
            let p_meta = tmpfs_guard_mut(&mut guards, &self_arc);
            if self_arc.is_detached() || p_meta.nlink == 0 {
                return Err(VfsError::NoEnt);
            }
            if p_meta.dir_mut()?.children.remove(name).is_none() {
                return Err(VfsError::NoEnt);
            }
            p_meta.mtime = now;
            p_meta.ctime = now;
        }
        let c_meta = tmpfs_guard_mut(&mut guards, &child_arc);
        if c_meta.nlink <= 1 {
            Self::detach_if_open(&child_arc, c_meta);
            c_meta.ctime = now;
        } else {
            c_meta.nlink -= 1;
            c_meta.ctime = now;
        }
        Ok(())
    }

    fn rmdir(&self, name: &[u8]) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == b"." {
            return Err(VfsError::Inval);
        }
        if name == b".." {
            return Err(VfsError::NotEmpty);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let child_arc = {
            let meta = self_arc.meta.lock();
            if self_arc.is_detached() || meta.nlink == 0 {
                return Err(VfsError::NoEnt);
            }
            let c = meta.dir()?.children.get(name).ok_or(VfsError::NoEnt)?;
            if c.inode_type != InodeType::Dir {
                return Err(VfsError::NotDir);
            }
            Arc::clone(c)
        };

        let mut guards = lock_tmpfs_ordered(&[&self_arc, &child_arc]);
        if !tmpfs_guard_mut(&mut guards, &child_arc).dir()?.children.is_empty() {
            return Err(VfsError::NotEmpty);
        }
        let now = time::realtime_timespec();
        {
            let p_meta = tmpfs_guard_mut(&mut guards, &self_arc);
            if self_arc.is_detached() || p_meta.nlink == 0 {
                return Err(VfsError::NoEnt);
            }
            if p_meta.dir_mut()?.children.remove(name).is_none() {
                return Err(VfsError::NoEnt);
            }
            p_meta.nlink = p_meta.nlink.saturating_sub(1);
            p_meta.mtime = now;
            p_meta.ctime = now;
        }
        let c_meta = tmpfs_guard_mut(&mut guards, &child_arc);
        Self::detach_if_open(&child_arc, c_meta);
        c_meta.ctime = now;
        Ok(())
    }

    fn link(&self, name: &[u8], target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if target.dev_id() != self.shared.dev {
            return Err(VfsError::XDev);
        }
        if target.inode_type() == InodeType::Dir {
            return Err(VfsError::Perm);
        }
        if target.is_detached() {
            return Err(VfsError::NoEnt);
        }
        Self::validate_name(name)?;
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        // Locate target `Arc<TmpfsInode>` from root tree.
        let target_arc = find_tmpfs_non_dir(&self_arc, target.lock_id()).ok_or(VfsError::NoEnt)?;
        let mut guards = lock_tmpfs_ordered(&[&self_arc, &target_arc]);
        if self_arc.is_detached()
            || target_arc.is_detached()
            || tmpfs_guard_mut(&mut guards, &self_arc).nlink == 0
            || tmpfs_guard_mut(&mut guards, &target_arc).nlink == 0
        {
            return Err(VfsError::NoEnt);
        }
        let now = time::realtime_timespec();
        {
            let p_meta = tmpfs_guard_mut(&mut guards, &self_arc);
            let children = &mut p_meta.dir_mut()?.children;
            let entry = children.reserve_new(name)?;
            children.add(Arc::clone(&target_arc), entry);
            p_meta.mtime = now;
            p_meta.ctime = now;
        }
        let t_meta = tmpfs_guard_mut(&mut guards, &target_arc);
        t_meta.nlink = t_meta.nlink.saturating_add(1);
        t_meta.ctime = now;
        Ok(())
    }

    fn symlink(&self, name: &[u8], target: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if target.is_empty() {
            return Err(VfsError::NoEnt);
        }
        if target.len() >= lumen_vfs::PATH_MAX {
            return Err(VfsError::NameTooLong);
        }
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let children = &mut meta.dir_mut()?.children;
        let entry = children.reserve_new(name)?;
        let child = Self::new_child(&self.shared, Node::Symlink(Box::from(target)), 0o777, 1)?;
        children.add(Arc::clone(&child), entry);
        let now = time::realtime_timespec();
        meta.mtime = now;
        meta.ctime = now;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        match &self.meta.lock().node {
            Node::Symlink(target) => Ok(target.to_vec()),
            Node::File | Node::Dir(_) => Err(VfsError::Inval),
        }
    }

    fn rename(
        &self,
        old_name: &[u8],
        new_dir: &Arc<dyn VfsInode>,
        new_name: &[u8],
        flags: RenameFlags,
    ) -> Result<(), VfsError> {
        let flags = flags.validate()?;
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if new_dir.dev_id() != self.shared.dev {
            return Err(VfsError::XDev);
        }
        if new_dir.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(old_name)?;
        Self::validate_name(new_name)?;
        if old_name == b"." || old_name == b".." || new_name == b"." || new_name == b".." {
            return Err(VfsError::Inval);
        }

        let old_dir_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let new_dir_arc = find_tmpfs_dir_arc(self, new_dir.lock_id(), new_dir.ino()).ok_or(VfsError::NoEnt)?;

        let (old_child_arc, new_child_opt) = {
            let mut dir_guards = lock_tmpfs_ordered(&[&old_dir_arc, &new_dir_arc]);
            if old_dir_arc.is_detached()
                || new_dir_arc.is_detached()
                || tmpfs_guard_mut(&mut dir_guards, &old_dir_arc).nlink == 0
                || tmpfs_guard_mut(&mut dir_guards, &new_dir_arc).nlink == 0
            {
                return Err(VfsError::NoEnt);
            }
            let old_c = tmpfs_guard_mut(&mut dir_guards, &old_dir_arc)
                .dir()?
                .children
                .get(old_name)
                .cloned()
                .ok_or(VfsError::NoEnt)?;
            let new_c = tmpfs_guard_mut(&mut dir_guards, &new_dir_arc).dir()?.children.get(new_name).cloned();
            (old_c, new_c)
        };

        if flags.has_noreplace() && new_child_opt.is_some() {
            return Err(VfsError::Exist);
        }
        if flags.has_exchange() && new_child_opt.is_none() {
            return Err(VfsError::NoEnt);
        }
        if old_dir_arc.lock_id == new_dir_arc.lock_id && old_name == new_name {
            return Ok(());
        }

        let mut to_lock = alloc::vec![&old_dir_arc, &new_dir_arc, &old_child_arc];
        if let Some(ref nc) = new_child_opt {
            to_lock.push(nc);
        }
        let mut guards = lock_tmpfs_ordered(&to_lock);
        if old_dir_arc.is_detached()
            || new_dir_arc.is_detached()
            || tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink == 0
            || tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink == 0
        {
            return Err(VfsError::NoEnt);
        }

        let now = time::realtime_timespec();
        let same_parent = old_dir_arc.lock_id == new_dir_arc.lock_id;
        let old_is_dir = old_child_arc.inode_type == InodeType::Dir;

        if flags.has_exchange() {
            let new_child_arc = new_child_opt.as_ref().expect("checked Some for EXCHANGE");
            let new_is_dir = new_child_arc.inode_type == InodeType::Dir;
            // Both names stay, each keeping its charge.
            {
                let old_p = tmpfs_guard_mut(&mut guards, &old_dir_arc);
                old_p.known_dir_mut().children.replace(old_name, Arc::clone(new_child_arc));
                old_p.mtime = now;
                old_p.ctime = now;
            }
            {
                let new_p = tmpfs_guard_mut(&mut guards, &new_dir_arc);
                new_p.known_dir_mut().children.replace(new_name, Arc::clone(&old_child_arc));
                new_p.mtime = now;
                new_p.ctime = now;
            }
            if !same_parent {
                if old_is_dir {
                    tmpfs_guard_mut(&mut guards, &old_child_arc).known_dir_mut().parent = Arc::downgrade(&new_dir_arc);
                    tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink =
                        tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink.saturating_sub(1);
                    tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink =
                        tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink.saturating_add(1);
                }
                if new_is_dir {
                    tmpfs_guard_mut(&mut guards, new_child_arc).known_dir_mut().parent = Arc::downgrade(&old_dir_arc);
                    tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink =
                        tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink.saturating_sub(1);
                    tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink =
                        tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink.saturating_add(1);
                }
            }
            tmpfs_guard_mut(&mut guards, &old_child_arc).ctime = now;
            tmpfs_guard_mut(&mut guards, new_child_arc).ctime = now;
            return Ok(());
        }

        // Every check comes before the first change. A new name takes the
        // old name's charge, and only what it costs more is reserved, here:
        // on a full mount a name no costlier than the old one still fits.
        // A name replaced keeps its charge, and the old name's is given back.
        let new_is_dir = new_child_opt.as_ref().is_some_and(|nc| nc.inode_type == InodeType::Dir);
        let growth = match &new_child_opt {
            None => Some(Growth::reserve(&self.shared.quota, old_name, new_name)?),
            Some(nc) => {
                if nc.lock_id == old_child_arc.lock_id {
                    return Ok(());
                }
                if old_is_dir && !new_is_dir {
                    return Err(VfsError::NotDir);
                }
                if !old_is_dir && new_is_dir {
                    return Err(VfsError::IsDir);
                }
                if new_is_dir && !tmpfs_guard_mut(&mut guards, nc).dir()?.children.is_empty() {
                    return Err(VfsError::NotEmpty);
                }
                None
            }
        };
        // The first change: the old name out, with its charge (there under
        // the rename lock; were it gone, nothing has changed yet).
        let (moved, entry) = tmpfs_guard_mut(&mut guards, &old_dir_arc)
            .known_dir_mut()
            .children
            .take(old_name)
            .ok_or(VfsError::NoEnt)?;
        debug_assert!(Arc::ptr_eq(&moved, &old_child_arc), "tmpfs: {:?} changed during a rename", Escaped(old_name));
        if let Some(ref nc) = new_child_opt {
            let nc_meta = tmpfs_guard_mut(&mut guards, nc);
            if new_is_dir || nc_meta.nlink <= 1 {
                Self::detach_if_open(nc, nc_meta);
            } else {
                nc_meta.nlink -= 1;
                nc_meta.ctime = now;
            }
            if new_is_dir {
                let np = tmpfs_guard_mut(&mut guards, &new_dir_arc);
                np.nlink = np.nlink.saturating_sub(1);
            }
        }
        let new_children = &mut tmpfs_guard_mut(&mut guards, &new_dir_arc).known_dir_mut().children;
        match growth {
            Some(growth) => new_children.add(moved, entry.renamed(growth)),
            None => {
                drop(entry);
                new_children.replace(new_name, moved);
            }
        }

        if old_is_dir && !same_parent {
            tmpfs_guard_mut(&mut guards, &old_child_arc).known_dir_mut().parent = Arc::downgrade(&new_dir_arc);
            tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink =
                tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink.saturating_sub(1);
            tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink =
                tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink.saturating_add(1);
        }
        tmpfs_guard_mut(&mut guards, &old_dir_arc).mtime = now;
        tmpfs_guard_mut(&mut guards, &old_dir_arc).ctime = now;
        tmpfs_guard_mut(&mut guards, &new_dir_arc).mtime = now;
        tmpfs_guard_mut(&mut guards, &new_dir_arc).ctime = now;
        tmpfs_guard_mut(&mut guards, &old_child_arc).ctime = now;
        Ok(())
    }

    /// The cursor is `.` (0), `..` (1), then each entry's sequence number
    /// ([`Children`]): stable as entries come and go, so a listing across
    /// removals lists every entry that stayed exactly once, in the order
    /// they were added.
    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 || max_entries == 0 {
            return Ok(());
        }
        let parent_ino = meta.parent().map_or(self.ino(), |p| p.ino());
        let dots = [(0, self.ino(), InodeType::Dir, &b"."[..]), (1, parent_ino, InodeType::Dir, &b".."[..])];
        let children = meta
            .dir()?
            .children
            .listed_from(cursor.max(FIRST_CHILD_CURSOR))
            .map(|(seq, name, child)| (seq, child.ino(), child.inode_type, name));
        for (seq, ino, inode_type, name) in dots.into_iter().filter(|&(seq, ..)| seq >= cursor).chain(children) {
            if out.len() >= max_entries {
                break;
            }
            out.push(DirEntry { ino, inode_type, name: name.to_vec(), next_cursor: seq + 1 });
        }
        Ok(())
    }

    fn truncate(&self, new_size: u64) -> Result<(), VfsError> {
        if self.inode_type == InodeType::Dir {
            return Err(VfsError::IsDir);
        }
        if self.inode_type != InodeType::File {
            return Err(VfsError::Inval);
        }
        let mut meta = self.meta.lock();
        let mut pages = self.pages.lock();
        let old_size = meta.size;
        if new_size < old_size {
            pages.remove_from(new_size.div_ceil(PAGE_SIZE as u64));
            let tail_rem = (new_size % PAGE_SIZE as u64) as usize;
            if tail_rem != 0
                && let Some(page) = pages.get(new_size / PAGE_SIZE as u64)
            {
                page.zero_range(tail_rem, PAGE_SIZE);
            }
        } else if new_size > old_size {
            let tail_rem = (old_size % PAGE_SIZE as u64) as usize;
            if tail_rem != 0
                && let Some(page) = pages.get(old_size / PAGE_SIZE as u64)
            {
                page.zero_range(tail_rem, PAGE_SIZE);
            }
        }
        let now = time::realtime_timespec();
        meta.size = new_size;
        meta.mtime = now;
        meta.ctime = now;
        Ok(())
    }

    fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        meta.mode = mode & 0o7777;
        meta.ctime = time::realtime_timespec();
        Ok(())
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        if let Some(u) = uid {
            meta.uid = u;
        }
        if let Some(g) = gid {
            meta.gid = g;
        }
        // In the same hold (`VfsInode::set_owner`'s contract).
        meta.mode = mode_after_chown(self.inode_type, meta.mode);
        meta.ctime = time::realtime_timespec();
        Ok(())
    }

    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        let now = time::realtime_timespec();
        match atime {
            SetTime::Omit => {}
            SetTime::Now => meta.atime = now,
            SetTime::Set(ts) => meta.atime = ts,
        }
        match mtime {
            SetTime::Omit => {}
            SetTime::Now => meta.mtime = now,
            SetTime::Set(ts) => meta.mtime = ts,
        }
        meta.ctime = now;
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        if self.inode_type == InodeType::Dir {
            return Err(VfsError::IsDir);
        }
        if self.inode_type != InodeType::File {
            return Err(VfsError::Inval);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let meta = self.meta.lock();
        if offset >= meta.size {
            return Ok(0);
        }
        let to_read = buf.len().min((meta.size - offset) as usize);
        let pages = self.pages.lock();
        let mut done = 0usize;
        while done < to_read {
            let cur_off = offset + done as u64;
            let page_idx = cur_off / PAGE_SIZE as u64;
            let in_page = (cur_off % PAGE_SIZE as u64) as usize;
            let chunk = (PAGE_SIZE - in_page).min(to_read - done);
            if let Some(page) = pages.get(page_idx) {
                page.read_bytes(in_page, &mut buf[done..done + chunk]);
            } else {
                // Sparse miss: zero without allocating a frame.
                buf[done..done + chunk].fill(0);
            }
            done += chunk;
        }
        Ok(to_read)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        self.write_locked(Some(offset), buf).map(|(_, n)| n)
    }

    fn append(&self, buf: &[u8]) -> Result<(u64, usize), VfsError> {
        self.write_locked(None, buf)
    }

    fn read_page(&self, page_index: u64, buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
        let off = page_index.checked_mul(PAGE_SIZE as u64).ok_or(VfsError::Inval)?;
        self.read_at(off, buf)
    }

    fn write_page(&self, page_index: u64, buf: &[u8]) -> Result<(), VfsError> {
        let off = page_index.checked_mul(PAGE_SIZE as u64).ok_or(VfsError::Inval)?;
        let _ = self.write_at(off, buf)?;
        Ok(())
    }

    fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
        Ok(())
    }

    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        f(&mut meta.locks);
        Ok(())
    }

    /// For `vfs::mmap_page`.
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

fn find_tmpfs_non_dir(known_dir: &Arc<TmpfsInode>, target_lock_id: u64) -> Option<Arc<TmpfsInode>> {
    let mut cur = Arc::clone(known_dir);
    loop {
        let next = cur.meta.lock().parent();
        match next {
            Some(p) if p.lock_id != cur.lock_id => cur = p,
            _ => break,
        }
    }
    let mut stack = alloc::vec![cur];
    while let Some(node) = stack.pop() {
        let m = node.meta.lock();
        let Ok(dir) = m.dir() else { continue };
        for child in dir.children.values() {
            if child.lock_id == target_lock_id {
                return Some(Arc::clone(child));
            }
            if child.inode_type == InodeType::Dir {
                stack.push(Arc::clone(child));
            }
        }
    }
    None
}

crate::ktest!(
    fn tmpfs_mmap_page_hands_out_the_cached_page() {
        // `vfs::mmap_page` on tmpfs: the page holding the file's bytes, the
        // same `Arc` again, coherent with `read_at`/`write_at` both ways; a
        // hole gets a zero page from the quota (`NoSpc` past it), which a
        // `read_at` of a hole never takes; `Range` past EOF, `NoDev` for a
        // directory. (The pages' bytes are used here without Lock 5, as a
        // mapping would: nothing else uses this mount.)
        use crate::vfs::mmap_page;
        use crate::{kassert, kassert_eq};
        let fs = Tmpfs::new(DevId::makedev(0, 125), 4 * PAGE_SIZE + KTEST_METADATA_ROOM);
        let root = fs.root_inode();
        let f = root.create(b"map.bin", 0o644).map_err(|_| "create")?;
        let data_pages = || (fs.used_bytes() - INODE_COST - entry_cost(7)) / PAGE_SIZE;
        let data: Vec<u8> = (0..PAGE_SIZE + 100).map(|i| (i % 251) as u8 | 1).collect();
        kassert_eq!(f.write_at(0, &data).map_err(|_| "write")?, data.len());
        kassert_eq!(data_pages(), 2);
        let first = mmap_page(&f, 0).map_err(|_| "map page 0")?;
        kassert_eq!(first.file_size(), data.len() as u64);
        let mut got = [0u8; PAGE_SIZE];
        first.page().read_bytes(0, &mut got);
        kassert!(got[..] == data[..PAGE_SIZE]);
        let last = mmap_page(&f, 1).map_err(|_| "map page 1")?;
        last.page().read_bytes(0, &mut got);
        kassert!(got[..100] == data[PAGE_SIZE..]);
        kassert!(got[100..].iter().all(|&b| b == 0)); // past EOF: zero
        let again = mmap_page(&f, 0).map_err(|_| "map page 0 again")?;
        kassert!(Arc::ptr_eq(again.page(), first.page()));
        kassert_eq!(data_pages(), 2); // no page added for either
        // Coherent both ways: a write shows in the page, a store in the
        // page shows in a read.
        kassert_eq!(f.write_at(5, b"written").map_err(|_| "write_at")?, 7);
        first.page().read_bytes(5, &mut got[..7]);
        kassert!(&got[..7] == b"written");
        first.page().write_bytes(20, b"stored");
        let mut back = [0u8; 6];
        kassert_eq!(f.read_at(20, &mut back).map_err(|_| "read_at")?, 6);
        kassert!(&back == b"stored");
        // Holes: a read leaves them be, a mapping takes a page each until
        // the quota (4 pages) refuses.
        f.truncate(5 * PAGE_SIZE as u64).map_err(|_| "truncate")?;
        kassert_eq!(f.read_at(2 * PAGE_SIZE as u64, &mut back).map_err(|_| "read hole")?, 6);
        kassert_eq!(data_pages(), 2);
        let hole = mmap_page(&f, 3).map_err(|_| "map hole")?;
        kassert_eq!(data_pages(), 3);
        hole.page().read_bytes(0, &mut got);
        kassert!(got.iter().all(|&b| b == 0));
        kassert!(Arc::ptr_eq(mmap_page(&f, 3).map_err(|_| "map hole again")?.page(), hole.page()));
        mmap_page(&f, 2).map_err(|_| "map last page of the quota")?;
        kassert_eq!(data_pages(), 4);
        kassert_eq!(mmap_page(&f, 4).err(), Some(VfsError::NoSpc));
        // Past EOF there is no page: `1 << 52` pages is 2^64 bytes, which a
        // plain multiplication wraps to offset 0. Nor in a directory or a
        // symlink.
        kassert_eq!(mmap_page(&f, 5).err(), Some(VfsError::Range));
        kassert_eq!(mmap_page(&f, 1 << 52).err(), Some(VfsError::Range));
        kassert_eq!(mmap_page(&f, u64::MAX).err(), Some(VfsError::Range));
        kassert_eq!(mmap_page(&root, 0).err(), Some(VfsError::NoDev));
        let link = root.symlink(b"link", b"map.bin").map_err(|_| "symlink")?;
        kassert_eq!(mmap_page(&link, 0).err(), Some(VfsError::NoDev));
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_open_racing_unlink_keeps_the_data_or_fails_cleanly() {
        // #262 on tmpfs (`/tmp`, and the root without an ext4 disk): an
        // unlink landing between an open's path lookup and the open file
        // description it builds. With the open's reference already up the
        // unlink detaches; without it, the file is unlinked with no opener
        // (`nlink == 0`, not detached) and the descriptor is refused.
        use crate::vfs::{KernelVfsLock, KernelVfsTables, read_all_inode};
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{OpenFile, OpenFlags, open_inode, open_inode_guarded, unlink_at};
        // Each failure's `VfsError` goes to the TAP diagnostics.
        fn why(what: &'static str) -> impl FnOnce(VfsError) -> &'static str {
            move |e| {
                crate::ktest::diag(format_args!("{what}: {e:?}"));
                what
            }
        }
        let fs = Tmpfs::new(DevId::makedev(0, 122), 64 * PAGE_SIZE);
        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(why("mount_root"))?;
        let data: Vec<u8> = (0..2 * PAGE_SIZE + 100).map(|i| (i as u8).wrapping_mul(31) | 1).collect();
        for name in ["kept", "freed"] {
            let f = fs.root_inode().create(name.as_bytes(), 0o644).map_err(why("create"))?;
            kassert_eq!(f.write_at(0, &data).map_err(why("write"))?, data.len());
        }
        let rd = OpenFlags::from_bits_truncate(OpenFlags::O_RDONLY);

        let opened = open_inode_guarded(&tables, None, b"/kept", rd, 0, 0).map_err(why("open kept"))?;
        unlink_at(&tables, None, b"/kept", false).map_err(why("unlink kept"))?;
        let of = OpenFile::<KernelVfsLock<u64>>::from_opened(opened).map_err(why("OpenFile::from_opened"))?;
        kassert!(of.inode.is_detached());
        kassert_eq!(of.inode.stat().map_err(why("stat kept"))?.nlink, 0);
        kassert_eq!(of.inode.open_count(), 1);
        kassert_eq!(read_all_inode(&of.inode).map_err(why("read kept"))?, data);
        let kept = Arc::clone(&of.inode);
        drop(of);
        kassert_eq!(kept.open_count(), 0);

        let (inode, path) = open_inode(&tables, None, b"/freed", rd, 0, 0).map_err(why("open freed"))?;
        kassert_eq!(inode.open_count(), 0);
        unlink_at(&tables, None, b"/freed", false).map_err(why("unlink freed"))?;
        kassert!(!inode.is_detached());
        kassert_eq!(inode.stat().map_err(why("stat freed"))?.nlink, 0);
        let res =
            OpenFile::<KernelVfsLock<u64>>::new_unchecked_flags(Arc::clone(&inode), Arc::from(path.as_slice()), rd);
        kassert_eq!(res.err(), Some(VfsError::NoEnt));
        kassert_eq!(inode.open_count(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_quota_charges_each_inode_and_refuses_past_it() {
        // #267: a tmpfs mount's quota pays for each inode and each directory
        // entry, not only for pages: past it creating or linking fails with
        // `NoSpc` and leaves nothing behind, an existing name is still
        // `Exist`, and freeing gives the room back exactly. Room for exactly
        // `N` files named with 8 bytes.
        use crate::{kassert, kassert_eq};
        use alloc::format;
        kassert!(INODE_COST >= size_of::<TmpfsInode>() + 2 * size_of::<usize>()); // an `Arc`'s block
        kassert_eq!(entry_cost(255) - entry_cost(0), 2 * 256); // two 16-byte-granule blocks
        const N: usize = 6;
        let file_cost = INODE_COST + entry_cost(8);
        let fs = Tmpfs::new(DevId::makedev(0, 135), N * file_cost);
        let root = fs.root_inode();
        let name = |i: usize| format!("f{i:07}").into_bytes();
        let mut held = Vec::new();
        for i in 0..N {
            held.push(root.create(&name(i), 0o644).map_err(|_| "create within the quota")?);
        }
        kassert_eq!(fs.used_bytes(), N * file_cost);
        let root_nlink = root.stat().map_err(|_| "stat root")?.nlink;
        kassert_eq!(root.create(b"one_more", 0o644).err(), Some(VfsError::NoSpc));
        kassert_eq!(root.mkdir(b"one_more", 0o755).err(), Some(VfsError::NoSpc));
        kassert_eq!(root.symlink(b"one_more", b"t").err(), Some(VfsError::NoSpc));
        kassert_eq!(root.link(b"one_more", &held[1]).err(), Some(VfsError::NoSpc));
        kassert_eq!(root.lookup(b"one_more").err(), Some(VfsError::NoEnt));
        kassert_eq!(fs.used_bytes(), N * file_cost);
        kassert_eq!(held[1].stat().map_err(|_| "stat")?.nlink, 1);
        // An existing name is `Exist`, full or not.
        kassert_eq!(root.create(&name(1), 0o644).err(), Some(VfsError::Exist));
        kassert_eq!(root.mkdir(&name(1), 0o755).err(), Some(VfsError::Exist));
        kassert_eq!(root.symlink(&name(1), b"t").err(), Some(VfsError::Exist));
        kassert_eq!(root.link(&name(1), &held[2]).err(), Some(VfsError::Exist));
        // Exchanging two names keeps both, and their charges.
        let exchange = RenameFlags(RenameFlags::EXCHANGE);
        root.rename(&name(2), &root, &name(3), exchange).map_err(|_| "exchange on a full mount")?;
        kassert_eq!(fs.used_bytes(), N * file_cost);
        // Unlinking gives the name's room back; the inode's comes back when
        // its last reference goes (an open file unlinked keeps it).
        root.unlink(&name(0)).map_err(|_| "unlink")?;
        kassert_eq!(fs.used_bytes(), N * file_cost - entry_cost(8));
        drop(held.remove(0));
        kassert_eq!(fs.used_bytes(), (N - 1) * file_cost);
        // A directory costs a file's room and its maps' (`DIR_COST`) too.
        kassert_eq!(root.mkdir(b"d0000000", 0o755).err(), Some(VfsError::NoSpc));
        kassert_eq!(root.lookup(b"d0000000").err(), Some(VfsError::NoEnt));
        kassert_eq!(fs.used_bytes(), (N - 1) * file_cost);
        kassert_eq!(root.stat().map_err(|_| "stat root")?.nlink, root_nlink); // no `..` counted
        held.insert(0, root.create(&name(0), 0o644).map_err(|_| "create in the room freed")?);
        kassert_eq!(fs.used_bytes(), N * file_cost);
        // A rename over a file frees the old name and the replaced inode.
        held.clear();
        root.rename(&name(1), &root, &name(2), RenameFlags(0)).map_err(|_| "rename over")?;
        kassert_eq!(fs.used_bytes(), (N - 1) * file_cost);
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_quota_charges_a_directory_its_maps() {
        // #267: a directory costs an inode, its maps' root nodes
        // (`DIR_COST`) and its name. Room for exactly one, `d`, with nothing
        // in it; and a byte less, none. Removing it gives its name back, and
        // its last reference the rest.
        use crate::kassert_eq;
        let dir_cost = INODE_COST + DIR_COST + entry_cost(1);
        {
            let fs = Tmpfs::new(DevId::makedev(0, 139), dir_cost - 1);
            kassert_eq!(fs.root_inode().mkdir(b"d", 0o755).err(), Some(VfsError::NoSpc));
            kassert_eq!(fs.used_bytes(), 0);
        }
        let fs = Tmpfs::new(DevId::makedev(0, 139), dir_cost);
        let root = fs.root_inode();
        let d = root.mkdir(b"d", 0o755).map_err(|_| "mkdir within the quota")?;
        kassert_eq!(fs.used_bytes(), dir_cost);
        kassert_eq!(d.create(b"x", 0o644).err(), Some(VfsError::NoSpc));
        kassert_eq!(d.symlink(b"x", b"y").err(), Some(VfsError::NoSpc));
        root.rmdir(b"d").map_err(|_| "rmdir")?;
        kassert_eq!(fs.used_bytes(), INODE_COST + DIR_COST); // `d` itself, still held
        drop(d);
        kassert_eq!(fs.used_bytes(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_quota_charges_names_by_length_and_renames_net() {
        // #267: a name costs by its length, and a rename to a new name
        // reserves only what it costs more. A file named "t", then room for
        // exactly `K` more entries with 255-byte names.
        use crate::{kassert, kassert_eq};
        let flags = RenameFlags(0);
        const K: usize = 4;
        let long = |c: u8| [c; 255];
        let fs = Tmpfs::new(DevId::makedev(0, 136), INODE_COST + entry_cost(1) + K * entry_cost(255));
        let root = fs.root_inode();
        let t = root.create(b"t", 0o644).map_err(|_| "create t")?;
        for c in 0..K as u8 {
            root.link(&long(b'a' + c), &t).map_err(|_| "link within the quota")?;
        }
        let full = fs.used_bytes();
        kassert_eq!(full, INODE_COST + entry_cost(1) + K * entry_cost(255));
        kassert_eq!(root.link(&long(b'z'), &t).err(), Some(VfsError::NoSpc));
        kassert_eq!(root.link(b"z", &t).err(), Some(VfsError::NoSpc));
        kassert_eq!(root.lookup(&long(b'z')).err(), Some(VfsError::NoEnt));
        kassert_eq!(t.stat().map_err(|_| "stat t")?.nlink, 1 + K as u32);
        // Renaming on a full mount: to a name no longer than the old one
        // fits (the old name's room pays for it), as does one within the
        // old name's heap block; to one costing more is `NoSpc`, nothing
        // changed.
        root.rename(&long(b'a'), &root, &long(b'y'), flags).map_err(|_| "rename, same length")?;
        kassert_eq!(fs.used_bytes(), full);
        root.rename(b"t", &root, b"tt", flags).map_err(|_| "rename within the block")?;
        root.rename(b"tt", &root, b"t", flags).map_err(|_| "rename back")?;
        kassert_eq!(fs.used_bytes(), full);
        let longer = [b'u'; 17]; // two heap granules to "t"'s one
        kassert_eq!(root.rename(b"t", &root, &longer, flags).err(), Some(VfsError::NoSpc));
        kassert!(root.lookup(b"t").is_ok());
        kassert_eq!(root.lookup(&longer).err(), Some(VfsError::NoEnt));
        kassert_eq!(fs.used_bytes(), full);
        root.rename(&long(b'y'), &root, b"y", flags).map_err(|_| "rename shorter")?;
        kassert_eq!(fs.used_bytes(), full - entry_cost(255) + entry_cost(1));
        root.unlink(b"y").map_err(|_| "unlink y")?;
        root.link(&long(b'z'), &t).map_err(|_| "link in the room freed")?;
        kassert_eq!(fs.used_bytes(), full);
        for c in (1..K as u8).map(|c| b'a' + c).chain(*b"z") {
            root.unlink(&long(c)).map_err(|_| "unlink a link")?;
        }
        let one = INODE_COST + entry_cost(1);
        kassert_eq!(fs.used_bytes(), one);
        // Names at the heap granule's edge, 16 bytes and 17, each give back
        // exactly what it took.
        root.link(&[b'g'; 16], &t).map_err(|_| "link 16")?;
        root.link(&[b'h'; 17], &t).map_err(|_| "link 17")?;
        kassert_eq!(fs.used_bytes(), one + entry_cost(16) + entry_cost(17));
        root.unlink(&[b'g'; 16]).map_err(|_| "unlink 16")?;
        kassert_eq!(fs.used_bytes(), one + entry_cost(17));
        root.unlink(&[b'h'; 17]).map_err(|_| "unlink 17")?;
        kassert_eq!(fs.used_bytes(), one);
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_quota_charges_symlink_targets() {
        // #267: a symlink's target is charged with its inode. Room for one
        // symlink "s" to a 4000-byte target.
        use crate::{kassert, kassert_eq};
        const T: usize = 4000;
        let fs = Tmpfs::new(DevId::makedev(0, 137), INODE_COST + entry_cost(1) + T);
        let root = fs.root_inode();
        let target = [b'p'; T];
        let s = root.symlink(b"s", &target).map_err(|_| "symlink within the quota")?;
        kassert_eq!(fs.used_bytes(), INODE_COST + entry_cost(1) + T);
        kassert!(s.readlink().map_err(|_| "readlink")? == target);
        kassert_eq!(root.symlink(b"r", b"x").err(), Some(VfsError::NoSpc));
        root.unlink(b"s").map_err(|_| "unlink s")?;
        drop(s);
        kassert_eq!(fs.used_bytes(), 0);
        kassert_eq!(root.symlink(b"s", &[b'p'; T + 1]).err(), Some(VfsError::NoSpc));
        kassert_eq!(root.lookup(b"s").err(), Some(VfsError::NoEnt));
        kassert_eq!(fs.used_bytes(), 0);
        root.symlink(b"s", &target).map_err(|_| "symlink again")?;
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_quota_moves_charges_across_directories_and_drops_them_with_the_mount() {
        // #267: a rename across directories moves the name's charge, net of
        // the new name's cost, and an exchange moves nothing. Then dropping
        // the mount with everything in it gives every byte back but what an
        // inode still held keeps: pages, inodes, a directory's entries
        // inside (16 and 17 bytes among them), a target; and then the test's
        // own reference is the quota's last.
        use crate::kassert_eq;
        let flags = RenameFlags(0);
        let exchange = RenameFlags(RenameFlags::EXCHANGE);
        let fs = Tmpfs::new(DevId::makedev(0, 138), 64 * PAGE_SIZE);
        let quota = Arc::clone(&fs.shared.quota);
        let root = fs.root_inode();
        let sub = root.mkdir(b"sub", 0o755).map_err(|_| "mkdir sub")?;
        let f = sub.create(b"file", 0o644).map_err(|_| "create sub/file")?;
        kassert_eq!(f.write_at(0, &[1u8; 100]).map_err(|_| "write")?, 100);
        sub.symlink(b"link", b"file").map_err(|_| "symlink sub/link")?;
        let inodes = 3 * INODE_COST + DIR_COST;
        let target_block = heap_block(b"file".len());
        let at_first = inodes + target_block + entry_cost(3) + 2 * entry_cost(4) + PAGE_SIZE;
        kassert_eq!(quota.used_bytes(), at_first);
        sub.rename(b"file", &root, &[b'm'; 17], flags).map_err(|_| "rename across")?;
        kassert_eq!(quota.used_bytes(), at_first - entry_cost(4) + entry_cost(17));
        root.rename(&[b'm'; 17], &sub, b"link", exchange).map_err(|_| "exchange across")?;
        kassert_eq!(quota.used_bytes(), at_first - entry_cost(4) + entry_cost(17));
        sub.rename(b"link", &sub, &[b'g'; 16], flags).map_err(|_| "rename in sub")?;
        root.rename(&[b'm'; 17], &sub, &[b'h'; 17], flags).map_err(|_| "rename back across")?;
        let sub_names = entry_cost(16) + entry_cost(17);
        kassert_eq!(quota.used_bytes(), at_first - 2 * entry_cost(4) + sub_names);
        drop((sub, root, fs));
        kassert_eq!(quota.used_bytes(), INODE_COST + PAGE_SIZE); // `f` and its page
        drop(f);
        kassert_eq!(quota.used_bytes(), 0);
        kassert_eq!(Arc::strong_count(&quota), 1);
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_quota_dropped_with_bytes_reserved_logs_the_leak() {
        // A mount's quota dropped with bytes still reserved (an owner that
        // dropped without giving them back) logs the leak, naming the
        // mount: the line is pinned in test_ktest.py's `VFS_EXACT_LINES`,
        // so any tmpfs ktest mount that leaks fails there. The quota goes
        // with the mount's last owner.
        use crate::kassert;
        let fs = Tmpfs::new(DevId::makedev(0, 143), PAGE_SIZE);
        let quota = Arc::downgrade(&fs.shared.quota);
        kassert!(fs.shared.quota.try_reserve_bytes(16));
        drop(fs);
        kassert!(quota.upgrade().is_none());
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_quota_charges_cover_what_the_heap_gives() {
        // #267's bound: what a mount charges for inodes, entries and targets
        // is at least what they take from the kernel heap
        // (`heap::stats().used`, which nothing else moves meanwhile: the
        // other CPUs idle through the ktests, as `app::tests`' exact heap
        // checks also rely on, and names, targets and the holder are made
        // first). Four shapes:
        // - one directory with hundreds of long names, so both maps grow
        //   nodes past their roots (an entry's share);
        // - the same after removing every other name (std merges and
        //   refills the nodes that underflow, so this isn't their emptiest);
        // - many directories of one entry each (a directory's map roots,
        //   `DIR_COST`);
        // - one directory's leaves at std's minimum fill, `B - 1` entries
        //   each, where an entry's share of the nodes is the largest.
        // The root's own maps are free by design, so each baseline is taken
        // with the root already holding an entry.
        use crate::mm::heap;
        use crate::{kassert, kassert_eq};
        use alloc::format;
        fn covered(what: &'static str, heap0: usize, quota0: usize, fs: &Tmpfs) -> Result<(), &'static str> {
            let took = heap::stats().used as isize - heap0 as isize;
            let charged = fs.used_bytes() as isize - quota0 as isize;
            crate::ktest::diag(format_args!("{what}: the heap gave {took} bytes, the quota charged {charged}"));
            kassert!(took <= charged);
            Ok(())
        }
        const FILES: usize = 240;
        const LINKS: usize = 120;
        const DIRS: usize = 48;
        const MIN_FILL: usize = 700;
        let fs = Tmpfs::new(DevId::makedev(0, 141), 1024 * PAGE_SIZE);
        let root = fs.root_inode();
        root.create(b"a", 0o644).map_err(|_| "create a")?;
        // Zero-padded numbers, so a directory's names sort in the order
        // they are added: both maps then have the same shape.
        let numbered = |i: usize, len: usize| {
            let mut name = format!("{i:05}").into_bytes();
            name.resize(len, b'n');
            name
        };
        let names: Vec<Vec<u8>> = (0..FILES + LINKS).map(|i| numbered(i, 255)).collect();
        let dir_names: Vec<Vec<u8>> = (0..DIRS).map(|i| format!("d{i:04}").into_bytes()).collect();
        let min_names: Vec<Vec<u8>> = (0..MIN_FILL).map(|i| numbered(i, 16)).collect();
        crate::ktest::diag(format_args!(
            "INODE_COST {INODE_COST}, DIR_COST {DIR_COST}, entry_cost(8) {}, entry_cost(255) {}",
            entry_cost(8),
            entry_cost(255)
        ));
        let target = [b'p'; 4001]; // not a whole number of heap granules
        let mut files = Vec::with_capacity(FILES);
        let mut dirs = Vec::with_capacity(DIRS);

        let (heap0, quota0) = (heap::stats().used, fs.used_bytes());
        let big = root.mkdir(b"big", 0o755).map_err(|_| "mkdir big")?;
        for name in &names[..FILES] {
            files.push(big.create(name, 0o644).map_err(|_| "create")?);
        }
        for (i, name) in names[FILES..].iter().enumerate() {
            big.link(name, &files[i]).map_err(|_| "link")?;
        }
        big.symlink(b"s", &target).map_err(|_| "symlink")?;
        covered("one directory", heap0, quota0, &fs)?;
        for name in names.iter().step_by(2) {
            big.unlink(name).map_err(|_| "unlink")?;
        }
        covered("every other name removed", heap0, quota0, &fs)?;

        let (heap0, quota0) = (heap::stats().used, fs.used_bytes());
        for name in &dir_names {
            let d = root.mkdir(name, 0o755).map_err(|_| "mkdir")?;
            d.create(b"f", 0o644).map_err(|_| "create f")?;
            dirs.push(d);
        }
        covered("one entry per directory", heap0, quota0, &fs)?;

        // Names added in order fill each leaf to `2 * B - 1` and split it
        // keeping `B` on the left, one going up, the rest on the right; so
        // every leaf but the last holds `B` names and they start `B + 1`
        // names apart (std's `B` is 6). Unlinking the first name of each leaf after the
        // first leaves each at `B - 1`, std's minimum, with no merge: the
        // heap falls by exactly those names' blocks, no node freed (were
        // std's split to change, that fails first).
        let (heap0, quota0) = (heap::stats().used, fs.used_bytes());
        let min = root.mkdir(b"min", 0o755).map_err(|_| "mkdir min")?;
        let held = min.create(&min_names[0], 0o644).map_err(|_| "create in min")?;
        for name in &min_names[1..] {
            min.link(name, &held).map_err(|_| "link in min")?;
        }
        let filled = heap::stats().used;
        let mut removed = 0;
        for name in min_names.iter().skip(7).step_by(7) {
            min.unlink(name).map_err(|_| "unlink in min")?;
            removed += 1;
        }
        kassert_eq!(removed, MIN_FILL / 7 - 1);
        let fell = filled as isize - heap::stats().used as isize;
        kassert_eq!(fell, (removed * 2 * heap_block(16)) as isize);
        covered("leaves at minimum fill", heap0, quota0, &fs)?;
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_unlinking_through_the_vfs_gives_the_room_back() {
        // The charges come back through the kernel's VFS tables too, whose
        // caches let an unlinked inode go: files made by `open_inode` until
        // `NoSpc`, all unlinked by `unlink_at`, then as many made again.
        use crate::kassert_eq;
        use crate::vfs::KernelVfsTables;
        use alloc::format;
        use lumen_vfs::{OpenFlags, open_inode, unlink_at};
        const FILES: usize = 5;
        let file_cost = INODE_COST + entry_cost(8);
        let fs = Tmpfs::new(DevId::makedev(0, 142), FILES * file_cost);
        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;
        let create = OpenFlags::from_bits_truncate(OpenFlags::O_CREAT | OpenFlags::O_WRONLY);
        let path = |i: usize| format!("/f{i:07}").into_bytes();
        for _ in 0..2 {
            for i in 0..FILES {
                open_inode(&tables, None, &path(i), create, 0o644, 0).map_err(|_| "create within the quota")?;
            }
            kassert_eq!(open_inode(&tables, None, b"/one_more", create, 0o644, 0).err(), Some(VfsError::NoSpc));
            kassert_eq!(fs.used_bytes(), FILES * file_cost);
            for i in 0..FILES {
                unlink_at(&tables, None, &path(i), false).map_err(|_| "unlink")?;
            }
            kassert_eq!(fs.used_bytes(), 0);
        }
        Ok(())
    }
);
