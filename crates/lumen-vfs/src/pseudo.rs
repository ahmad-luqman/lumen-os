//! What the kernel's pseudo-filesystems whose names no caller can change
//! (devfs's fixed nodes, procfs's generated ones) share: [`fill_readdir`],
//! their listing, and [`fixed_namespace_ops!`](crate::fixed_namespace_ops),
//! their namespace operations, each `-EROFS`.

use alloc::vec::Vec;

use crate::types::{DirEntry, Ino, InodeType};

/// The `Arc` [`fixed_namespace_ops!`](crate::fixed_namespace_ops) names, so
/// its caller needs no `extern crate alloc`.
#[doc(hidden)]
pub use alloc::sync::Arc as __Arc;

/// `VfsInode::readdir` over generated entries: each `(key, (ino, type,
/// name))`, keys strictly ascending and each below `u64::MAX`, and each key
/// fixed for its entry (the caller's promise: an index into a list that
/// changes is not one). An entry is listed when its key is at least
/// `cursor`, with `next_cursor` its key plus 1 (so a cursor names an entry,
/// not a position, and a listing resumed after entries came and went lists
/// each one that stayed exactly once), until `out` holds `max_entries`
/// entries, counting those it held before.
pub fn fill_readdir<N: Into<Vec<u8>>>(
    out: &mut Vec<DirEntry>,
    cursor: u64,
    max_entries: usize,
    entries: impl IntoIterator<Item = (u64, (Ino, InodeType, N))>,
) {
    // Every key's order is checked, before the cursor and past a full page
    // too, so an order bug shows on any listing (Lumen's builds all keep
    // debug assertions, its release kernel too).
    let mut last = None;
    for (key, (ino, inode_type, name)) in entries {
        debug_assert!(last.is_none_or(|l| l < key), "readdir keys not ascending");
        last = Some(key);
        if key >= cursor && out.len() < max_entries {
            out.push(DirEntry { ino, inode_type, name: name.into(), next_cursor: key + 1 });
        }
    }
}

/// The seven namespace operations of a filesystem whose names no caller can
/// change (`create`, `mkdir`, `unlink`, `rmdir`, `link`, `symlink`,
/// `rename`), each `-EROFS`, inside its `impl VfsInode`.
#[macro_export]
macro_rules! fixed_namespace_ops {
    () => {
        fn create(
            &self,
            _name: &[u8],
            _mode: u16,
        ) -> Result<$crate::pseudo::__Arc<dyn $crate::VfsInode>, $crate::VfsError> {
            Err($crate::VfsError::RoFs)
        }

        fn mkdir(
            &self,
            _name: &[u8],
            _mode: u16,
        ) -> Result<$crate::pseudo::__Arc<dyn $crate::VfsInode>, $crate::VfsError> {
            Err($crate::VfsError::RoFs)
        }

        fn unlink(&self, _name: &[u8]) -> Result<(), $crate::VfsError> {
            Err($crate::VfsError::RoFs)
        }

        fn rmdir(&self, _name: &[u8]) -> Result<(), $crate::VfsError> {
            Err($crate::VfsError::RoFs)
        }

        fn link(
            &self,
            _name: &[u8],
            _target: &$crate::pseudo::__Arc<dyn $crate::VfsInode>,
        ) -> Result<(), $crate::VfsError> {
            Err($crate::VfsError::RoFs)
        }

        fn symlink(
            &self,
            _name: &[u8],
            _target: &[u8],
        ) -> Result<$crate::pseudo::__Arc<dyn $crate::VfsInode>, $crate::VfsError> {
            Err($crate::VfsError::RoFs)
        }

        fn rename(
            &self,
            _old_name: &[u8],
            _new_dir: &$crate::pseudo::__Arc<dyn $crate::VfsInode>,
            _new_name: &[u8],
            _flags: $crate::RenameFlags,
        ) -> Result<(), $crate::VfsError> {
            Err($crate::VfsError::RoFs)
        }
    };
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    /// What `fill_readdir` takes, as a list.
    type Entries = Vec<(u64, (Ino, InodeType, Vec<u8>))>;

    /// devfs-like keys (0, 1, 2) and procfs-like ones with a gap (0x103,
    /// 0x107), each named after its key.
    fn entries() -> Entries {
        [0u64, 1, 2, 0x103, 0x107]
            .into_iter()
            .map(|k| {
                (k, (k + 10, if k < 2 { InodeType::Dir } else { InodeType::File }, std::format!("{k}").into_bytes()))
            })
            .collect()
    }

    /// `keys`, each a file named after itself.
    fn keyed(keys: &[u64]) -> Entries {
        keys.iter().map(|&k| (k, (k + 10, InodeType::File, std::format!("{k}").into_bytes()))).collect()
    }

    fn listed(cursor: u64, max: usize) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        fill_readdir(&mut out, cursor, max, entries());
        out.into_iter().map(|e| (e.next_cursor, e.name)).collect()
    }

    fn names(keys: &[u64]) -> Vec<(u64, Vec<u8>)> {
        keys.iter().map(|&k| (k + 1, std::format!("{k}").into_bytes())).collect()
    }

    #[test]
    fn lists_every_entry_with_its_ino_type_and_the_cursor_past_it() {
        let mut out = Vec::new();
        fill_readdir(&mut out, 0, 64, entries());
        assert_eq!(out.len(), 5);
        assert_eq!(out[0], DirEntry { ino: 10, inode_type: InodeType::Dir, name: b"0".to_vec(), next_cursor: 1 });
        assert_eq!(
            out[4],
            DirEntry { ino: 0x107 + 10, inode_type: InodeType::File, name: b"263".to_vec(), next_cursor: 0x108 }
        );
    }

    #[test]
    fn a_cursor_names_an_entry_not_a_position() {
        assert_eq!(listed(2, 64), names(&[2, 0x103, 0x107]));
        // A cursor between two keys (an entry gone since) resumes at the next.
        assert_eq!(listed(0x104, 64), names(&[0x107]));
        assert_eq!(listed(0x103, 64), names(&[0x103, 0x107]));
        assert_eq!(listed(0x108, 64), names(&[]));
        assert_eq!(listed(u64::MAX, 64), names(&[]));
    }

    #[test]
    fn stops_once_out_holds_max_entries_counting_what_it_held() {
        assert_eq!(listed(0, 2), names(&[0, 1]));
        assert_eq!(listed(2, 2), names(&[2, 0x103]));
        assert_eq!(listed(0, 0), names(&[]));
        let held = DirEntry { ino: 1, inode_type: InodeType::Dir, name: b"held".to_vec(), next_cursor: 9 };
        let mut out = std::vec![held.clone(), held.clone()];
        fill_readdir(&mut out, 1, 3, entries());
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].name, b"1");
        let mut full = std::vec![held.clone(); 3];
        fill_readdir(&mut full, 0, 3, entries());
        assert_eq!(full, std::vec![held; 3]);
    }

    #[test]
    #[should_panic(expected = "readdir keys not ascending")]
    fn keys_out_of_order_past_a_full_page_still_panic() {
        fill_readdir(&mut Vec::new(), 0, 1, keyed(&[0, 5, 3]));
    }

    #[test]
    #[should_panic(expected = "readdir keys not ascending")]
    fn keys_out_of_order_before_the_cursor_still_panic() {
        fill_readdir(&mut Vec::new(), 6, 64, keyed(&[5, 3, 7]));
    }

    #[test]
    #[should_panic(expected = "readdir keys not ascending")]
    fn a_key_twice_panics() {
        // A resumed listing would skip the second: its cursor is past both.
        fill_readdir(&mut Vec::new(), 0, 64, keyed(&[2, 2]));
    }

    #[test]
    fn listing_in_pages_resumed_at_each_cursor_lists_each_entry_once() {
        for page in 1..=6 {
            let mut all = Vec::new();
            let mut cursor = 0;
            for call in 0.. {
                // Bounded: a cursor that never moves on fails, never hangs.
                assert!(call <= 5, "page {page}: no end after {call} calls");
                let mut out = Vec::new();
                fill_readdir(&mut out, cursor, page, entries());
                let Some(last) = out.last() else { break };
                cursor = last.next_cursor;
                all.extend(out.into_iter().map(|e| (e.next_cursor, e.name)));
            }
            assert_eq!(all, names(&[0, 1, 2, 0x103, 0x107]), "page {page}");
        }
    }
}
