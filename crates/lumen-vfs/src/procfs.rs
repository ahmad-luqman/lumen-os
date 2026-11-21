//! `/proc`'s node kinds, inode numbers and numeric names: the pure parts of
//! the kernel's procfs (`kernel/src/vfs/procfs.rs`).
//!
//! The fixed nodes have inode numbers 1..=6. A per-process node's is
//! `pid_ino`: bit 48 marks it (`DETACHED_INO_BIT` is far above), bits 16..48
//! hold the pid and bits 0..16 the tag (`TAG_PID_DIR` and the others, or
//! `TAG_PID_FD_BASE` plus the fd). So every pid is distinct, and an fd keeps
//! its tag below 2^16 because a [`ProcFd`] is in `0..MAX_FDS` and
//! `MAX_FDS <= 0x10000 - TAG_PID_FD_BASE`, which a compile-time assertion
//! keeps. Only [`ProcKind::ino`] hands the numbers out.

use crate::DETACHED_INO_BIT;
use crate::MAX_FDS;
use crate::types::InodeType;

/// `/proc` itself.
const INO_ROOT: u64 = 1;
/// `/proc/cpuinfo`.
const INO_CPUINFO: u64 = 2;
/// `/proc/meminfo`.
const INO_MEMINFO: u64 = 3;
/// `/proc/mounts`.
const INO_MOUNTS: u64 = 4;
/// `/proc/version`.
const INO_VERSION: u64 = 5;
/// `/proc/self`.
const INO_SELF: u64 = 6;

/// `/proc/<pid>`'s tag.
const TAG_PID_DIR: u64 = 0;
/// `/proc/<pid>/exe`'s tag.
const TAG_PID_EXE: u64 = 1;
/// `/proc/<pid>/maps`'s tag.
const TAG_PID_MAPS: u64 = 2;
/// `/proc/<pid>/fd`'s tag.
const TAG_PID_FD_DIR: u64 = 3;
/// `/proc/<pid>/mounts`'s tag.
const TAG_PID_MOUNTS: u64 = 4;
/// `/proc/<pid>/fd/<fd>`'s tag is this plus the fd.
const TAG_PID_FD_BASE: u64 = 0x1000;

/// The inode number of `pid`'s node with tag `tag` (see the module docs).
/// `tag` is below 2^16: a `TAG_PID_*`, or `TAG_PID_FD_BASE` plus a [`ProcFd`].
const fn pid_ino(pid: u32, tag: u64) -> u64 {
    (1u64 << 48) | ((pid as u64) << 16) | tag
}

const _: () = {
    // An fd's tag stays below 2^16, and apart from the other tags.
    assert!(MAX_FDS as u64 <= 0x1_0000 - TAG_PID_FD_BASE);
    assert!(TAG_PID_MOUNTS < TAG_PID_FD_BASE);
    // The fixed nodes are below every per-process one, and the largest
    // per-process one is below a detached inode's tag bit.
    assert!(INO_SELF < pid_ino(0, 0));
    assert!(pid_ino(u32::MAX, 0xffff) < DETACHED_INO_BIT);
};

/// A pid or fd name under `/proc`: ASCII digits only, without a sign or a
/// leading zero (but `0` itself), that fit a `u32`. So each number has one
/// name.
pub fn parse_canonical_u32(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse().ok()
}

/// An fd `/proc/<pid>/fd` can show: one in `0..MAX_FDS`, the range a
/// process's fd table keeps. Made only by [`ProcFd::try_from`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProcFd(u16);

/// An fd outside `0..MAX_FDS`, refused by [`ProcFd::try_from`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FdOutOfRange(pub i32);

const _: () = assert!(MAX_FDS <= u16::MAX as usize + 1);

impl ProcFd {
    /// The fd.
    pub const fn get(self) -> i32 {
        self.0 as i32
    }
}

impl TryFrom<i32> for ProcFd {
    type Error = FdOutOfRange;

    fn try_from(fd: i32) -> Result<Self, FdOutOfRange> {
        match u16::try_from(fd) {
            Ok(v) if (v as usize) < MAX_FDS => Ok(Self(v)),
            _ => Err(FdOutOfRange(fd)),
        }
    }
}

/// What a procfs node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcKind {
    /// `/proc`.
    Root,
    /// `/proc/cpuinfo`.
    Cpuinfo,
    /// `/proc/meminfo`.
    Meminfo,
    /// `/proc/mounts`.
    Mounts,
    /// `/proc/version`.
    Version,
    /// `/proc/self`, a symlink to the caller's `<pid>`.
    SelfLink,
    /// `/proc/<pid>`.
    PidDir(u32),
    /// `/proc/<pid>/exe`.
    PidExe(u32),
    /// `/proc/<pid>/maps`.
    PidMaps(u32),
    /// `/proc/<pid>/fd`.
    PidFdDir(u32),
    /// `/proc/<pid>/mounts`.
    PidMounts(u32),
    /// `/proc/<pid>/fd/<fd>`.
    PidFdLink(u32, ProcFd),
}

impl ProcKind {
    /// The node's inode number (see the module docs).
    pub const fn ino(self) -> u64 {
        match self {
            Self::Root => INO_ROOT,
            Self::Cpuinfo => INO_CPUINFO,
            Self::Meminfo => INO_MEMINFO,
            Self::Mounts => INO_MOUNTS,
            Self::Version => INO_VERSION,
            Self::SelfLink => INO_SELF,
            Self::PidDir(p) => pid_ino(p, TAG_PID_DIR),
            Self::PidExe(p) => pid_ino(p, TAG_PID_EXE),
            Self::PidMaps(p) => pid_ino(p, TAG_PID_MAPS),
            Self::PidFdDir(p) => pid_ino(p, TAG_PID_FD_DIR),
            Self::PidMounts(p) => pid_ino(p, TAG_PID_MOUNTS),
            Self::PidFdLink(p, fd) => pid_ino(p, TAG_PID_FD_BASE + fd.0 as u64),
        }
    }

    /// Directory, symlink or regular file.
    pub const fn inode_type(self) -> InodeType {
        match self {
            Self::Root | Self::PidDir(_) | Self::PidFdDir(_) => InodeType::Dir,
            Self::SelfLink | Self::PidExe(_) | Self::PidFdLink(_, _) => InodeType::Symlink,
            Self::Cpuinfo | Self::Meminfo | Self::Mounts | Self::Version | Self::PidMaps(_) | Self::PidMounts(_) => {
                InodeType::File
            }
        }
    }

    /// Whether the VFS may keep this node in its dentry and inode caches.
    /// A per-process node goes when its process or fd does, and pids only
    /// grow, so caching them would keep stale names and gain an entry per
    /// (pid, tag) for good (#263). `self` names a different process for
    /// each caller, so it stays out too. Fixed per node: the VFS asks under
    /// Lock 9.
    pub const fn cacheable(self) -> bool {
        match self {
            Self::Root | Self::Cpuinfo | Self::Meminfo | Self::Mounts | Self::Version => true,
            Self::SelfLink
            | Self::PidDir(_)
            | Self::PidExe(_)
            | Self::PidMaps(_)
            | Self::PidFdDir(_)
            | Self::PidMounts(_)
            | Self::PidFdLink(_, _) => false,
        }
    }

    /// The node's permission bits: directories `0555`, symlinks `0777`,
    /// files `0444`.
    pub const fn mode(self) -> u16 {
        match self.inode_type() {
            InodeType::Dir => 0o555,
            InodeType::Symlink => 0o777,
            _ => 0o444,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::collections::BTreeSet;
    use std::vec::Vec;

    use super::*;

    const FIXED: [ProcKind; 6] =
        [ProcKind::Root, ProcKind::Cpuinfo, ProcKind::Meminfo, ProcKind::Mounts, ProcKind::Version, ProcKind::SelfLink];

    fn fd(n: i32) -> ProcFd {
        ProcFd::try_from(n).unwrap()
    }

    #[test]
    fn the_fixed_nodes_are_inodes_one_to_six() {
        let inos: Vec<u64> = FIXED.iter().map(|k| k.ino()).collect();
        assert_eq!(inos, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn pid_ino_packs_the_marker_the_pid_and_the_tag() {
        assert_eq!(pid_ino(0, 0), 0x0001_0000_0000_0000);
        assert_eq!(pid_ino(1, 0), 0x0001_0000_0001_0000);
        assert_eq!(pid_ino(1, 4), 0x0001_0000_0001_0004);
        // Pids above 2^16 stay distinct (the kernel ktest's pair).
        assert_eq!(ProcKind::PidDir(65537).ino(), 0x0001_0001_0001_0000);
        assert_ne!(ProcKind::PidDir(1).ino(), ProcKind::PidDir(65537).ino());
        // The largest pid and tag fill bits 0..49, below DETACHED_INO_BIT.
        assert_eq!(pid_ino(u32::MAX, 0xffff), 0x0001_ffff_ffff_ffff);
        assert_eq!(pid_ino(u32::MAX, 0xffff) & DETACHED_INO_BIT, 0);
    }

    #[test]
    fn each_per_process_node_has_its_own_tag() {
        let p = 7;
        let base = 0x0001_0000_0007_0000;
        assert_eq!(ProcKind::PidDir(p).ino(), base);
        assert_eq!(ProcKind::PidExe(p).ino(), base + 1);
        assert_eq!(ProcKind::PidMaps(p).ino(), base + 2);
        assert_eq!(ProcKind::PidFdDir(p).ino(), base + 3);
        assert_eq!(ProcKind::PidMounts(p).ino(), base + 4);
        assert_eq!(ProcKind::PidFdLink(p, fd(0)).ino(), base + 0x1000);
        assert_eq!(ProcKind::PidFdLink(p, fd(3)).ino(), base + 0x1003);
        assert_eq!(ProcKind::PidFdLink(p, fd(1023)).ino(), base + 0x13ff);
    }

    #[test]
    fn every_fd_tag_stays_in_the_tag_bits_and_apart_from_the_others() {
        // The compile-time bound, restated: the last fd's tag is below 2^16.
        assert!(TAG_PID_FD_BASE + MAX_FDS as u64 <= 0x1_0000);
        let pid = u32::MAX;
        let fixed_tags = [TAG_PID_DIR, TAG_PID_EXE, TAG_PID_MAPS, TAG_PID_FD_DIR, TAG_PID_MOUNTS];
        let mut seen = BTreeSet::new();
        for tag in fixed_tags {
            assert!(seen.insert(pid_ino(pid, tag)));
        }
        for n in 0..MAX_FDS as i32 {
            let ino = ProcKind::PidFdLink(pid, fd(n)).ino();
            // The pid's bits are untouched, and the tag reads back the fd.
            assert_eq!(ino >> 16, pid_ino(pid, 0) >> 16, "fd {n}");
            assert_eq!(ino & 0xffff, TAG_PID_FD_BASE + n as u64, "fd {n}");
            assert!(seen.insert(ino), "fd {n}");
        }
        // No per-process node is a fixed one.
        for kind in FIXED {
            assert!(!seen.contains(&kind.ino()));
        }
    }

    #[test]
    fn a_proc_fd_is_only_one_in_the_fd_table_range() {
        let last = MAX_FDS as i32 - 1;
        assert_eq!(ProcFd::try_from(0).map(ProcFd::get), Ok(0));
        assert_eq!(ProcFd::try_from(last).map(ProcFd::get), Ok(last));
        assert_eq!(ProcFd::try_from(MAX_FDS as i32), Err(FdOutOfRange(MAX_FDS as i32)));
        assert_eq!(ProcFd::try_from(-1), Err(FdOutOfRange(-1)));
        assert_eq!(ProcFd::try_from(i32::MIN), Err(FdOutOfRange(i32::MIN)));
        assert_eq!(ProcFd::try_from(0x1_0000), Err(FdOutOfRange(0x1_0000)));
        assert_eq!(ProcFd::try_from(i32::MAX), Err(FdOutOfRange(i32::MAX)));
    }

    #[test]
    fn each_kind_has_its_type_and_mode() {
        let p = 9;
        let expect = [
            (ProcKind::Root, InodeType::Dir, 0o555),
            (ProcKind::Cpuinfo, InodeType::File, 0o444),
            (ProcKind::Meminfo, InodeType::File, 0o444),
            (ProcKind::Mounts, InodeType::File, 0o444),
            (ProcKind::Version, InodeType::File, 0o444),
            (ProcKind::SelfLink, InodeType::Symlink, 0o777),
            (ProcKind::PidDir(p), InodeType::Dir, 0o555),
            (ProcKind::PidExe(p), InodeType::Symlink, 0o777),
            (ProcKind::PidMaps(p), InodeType::File, 0o444),
            (ProcKind::PidFdDir(p), InodeType::Dir, 0o555),
            (ProcKind::PidMounts(p), InodeType::File, 0o444),
            (ProcKind::PidFdLink(p, fd(0)), InodeType::Symlink, 0o777),
        ];
        for (kind, itype, mode) in expect {
            assert_eq!((kind.inode_type(), kind.mode()), (itype, mode), "{kind:?}");
        }
    }

    #[test]
    fn only_the_fixed_files_and_the_root_are_cacheable() {
        let p = 9;
        let expect = [
            (ProcKind::Root, true),
            (ProcKind::Cpuinfo, true),
            (ProcKind::Meminfo, true),
            (ProcKind::Mounts, true),
            (ProcKind::Version, true),
            (ProcKind::SelfLink, false),
            (ProcKind::PidDir(p), false),
            (ProcKind::PidExe(p), false),
            (ProcKind::PidMaps(p), false),
            (ProcKind::PidFdDir(p), false),
            (ProcKind::PidMounts(p), false),
            (ProcKind::PidFdLink(p, fd(0)), false),
        ];
        for (kind, cacheable) in expect {
            assert_eq!(kind.cacheable(), cacheable, "{kind:?}");
            // The kernel's ktest tells the uncacheable ones by number: `self`,
            // or one numbered from `/proc/0`'s up.
            let by_ino = kind.ino() == ProcKind::SelfLink.ino() || kind.ino() >= ProcKind::PidDir(0).ino();
            assert_eq!(by_ino, !cacheable, "{kind:?}");
        }
    }

    #[test]
    fn canonical_numbers_parse() {
        assert_eq!(parse_canonical_u32("0"), Some(0));
        assert_eq!(parse_canonical_u32("7"), Some(7));
        assert_eq!(parse_canonical_u32("10"), Some(10));
        assert_eq!(parse_canonical_u32("1023"), Some(1023));
        assert_eq!(parse_canonical_u32("4294967295"), Some(u32::MAX));
    }

    #[test]
    fn non_canonical_names_are_refused() {
        for name in [
            "",            // empty
            "+1",          // a sign `str::parse` would take
            "+0",          // a signed zero
            "-1",          // negative
            "01",          // a leading zero
            "00",          // zero twice
            "007",         // several leading zeros
            "4294967296",  // u32::MAX + 1
            "99999999999", // far past u32
            " 1",          // whitespace before
            "1 ",          // whitespace after
            "1a",          // a letter
            "0x1",         // hex
            "\u{0661}",    // a non-ASCII digit (Arabic-Indic one)
            "self",        // a fixed name
        ] {
            assert_eq!(parse_canonical_u32(name), None, "{name:?}");
        }
    }
}
