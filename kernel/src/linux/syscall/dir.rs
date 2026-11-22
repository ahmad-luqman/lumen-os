//! Working directory and directory reads: `getcwd` (M6), and M7's
//! `getdents64`, `chdir`, `fchdir`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, Ordering};

use lumen_vfs::{DirEntry, InodeType, OpenGuard, ResolveFlags, VfsError, VfsInode, VfsLock};

use super::at::{Dirfd, Fd, UserPath, UserPtr, start_dir};
use super::finish;
use crate::linux::process;
use crate::linux::{errno, general};
use crate::user::{CopyError, copy_to_user};

/// `getcwd(buf, size)` (79): the working directory's path, computed from
/// its inode (so a rename of it or an ancestor shows), with its NUL; its
/// length with the NUL. `-ENOENT` once the directory is unlinked (or the
/// process has none), `-ERANGE` if `size` can't hold it.
pub(super) fn sys_getcwd(buf: UserPtr, size: u64) -> u64 {
    finish(getcwd(buf, size))
}

fn getcwd(buf: UserPtr, size: u64) -> Result<u64, u32> {
    let (cwd, _) = process::current_cwd();
    let path = lumen_vfs::compute_cwd_path(crate::vfs::tables(), &cwd).map_err(lumen_vfs::VfsError::to_errno)?;
    drop(cwd);
    let mut bytes: Vec<u8> = path.into_bytes();
    bytes.push(0);
    if (bytes.len() as u64) > size {
        return Err(errno::ERANGE);
    }
    copy_to_user(buf.addr(), &bytes).map_err(CopyError::to_errno)?;
    Ok(bytes.len() as u64)
}

/// `struct linux_dirent64`'s fixed part: `d_ino` (8 bytes), `d_off` (8),
/// `d_reclen` (2), `d_type` (1); the name follows at once.
const DIRENT_HEADER: usize = offset_of!(general::linux_dirent64, d_name);

/// The smallest record: the header, a one-byte name and its NUL, rounded up
/// to 8 bytes (24).
const MIN_RECLEN: usize = dirent_reclen(1);

/// The most bytes one `getdents64` call fills (and so allocates in the
/// kernel), whatever `count` says: room for over a hundred records of the
/// longest name (255 bytes, 280 with its header and padding), so any entry
/// fits. A caller asking more gets a partial fill and the rest on its next
/// call.
const GETDENTS_MAX: usize = 32 * 1024;

/// A record's length for a name of `name_len` bytes: header, name and NUL,
/// rounded up to 8 bytes (Linux's `ALIGN(.., sizeof(u64))`).
const fn dirent_reclen(name_len: usize) -> usize {
    (DIRENT_HEADER + name_len + 1).next_multiple_of(8)
}

/// `d_type` for an inode type (`DT_*`, `<dirent.h>`).
fn d_type(t: InodeType) -> u8 {
    let dt = match t {
        InodeType::File => general::DT_REG,
        InodeType::Dir => general::DT_DIR,
        InodeType::Symlink => general::DT_LNK,
        InodeType::CharDevice => general::DT_CHR,
        InodeType::BlockDevice => general::DT_BLK,
        InodeType::Fifo => general::DT_FIFO,
        InodeType::Socket => general::DT_SOCK,
    };
    dt as u8
}

/// Append `entry`'s record to `buf` if `buf` stays within `limit` bytes:
/// whether it did. The padding after the name's NUL is zeroed, so no kernel
/// bytes reach user space.
fn push_dirent(buf: &mut Vec<u8>, entry: &DirEntry, limit: usize) -> bool {
    debug_assert!(limit <= GETDENTS_MAX, "a getdents64 fill of {limit} bytes, past GETDENTS_MAX");
    let reclen = dirent_reclen(entry.name.len());
    if buf.len() + reclen > limit {
        return false;
    }
    let start = buf.len();
    buf.extend_from_slice(&entry.ino.to_le_bytes());
    // Every filesystem's cursors are far below `i64::MAX` (an index, a
    // sequence number, a byte position); one past it would read as
    // negative to `seekdir`.
    debug_assert!(i64::try_from(entry.next_cursor).is_ok(), "a readdir cursor past i64::MAX");
    let d_off = i64::try_from(entry.next_cursor).unwrap_or(i64::MAX);
    buf.extend_from_slice(&d_off.to_le_bytes());
    // At most 19 + 255 + 1 rounded up for a VFS name, and never past
    // `limit`, at most `GETDENTS_MAX` (checked above), so it fits a `u16`.
    buf.extend_from_slice(&(reclen as u16).to_le_bytes());
    buf.push(d_type(entry.inode_type));
    buf.extend_from_slice(entry.name.as_bytes());
    buf.resize(start + reclen, 0);
    true
}

/// `getdents64(fd, dirp, count)` (217): the directory's next entries as
/// `struct linux_dirent64` records (`.` and `..` included, each 8-byte
/// aligned, its name NUL-terminated), as many whole ones as fit in `count`
/// bytes (at most [`GETDENTS_MAX`]); the bytes filled, `0` at the end.
///
/// - **Position:** the open file's `dir_cursor` (Lock 2), which `lseek`
///   moves too (`SEEK_SET` 0 rewinds; `rewinddir`, `seekdir`). It is the
///   filesystem's own `readdir` cursor, opaque here: a sequence number on
///   tmpfs, a byte position in the directory on ext4, an index on procfs
///   and devfs. Each record's `d_off` is the cursor just past it, so
///   `lseek(fd, d_off, SEEK_SET)` (`seekdir(telldir())`) resumes with the
///   entry after it. The cursor moves past the last whole record filled: an
///   entry that didn't fit comes back on the next call.
/// - **Errors:** `-EBADF` for a descriptor not open or opened `O_PATH`,
///   `-ENOTDIR` for one that isn't a directory, `-EINVAL` if `count` can't
///   hold the next entry, `-EFAULT` (what the copy says) if the records
///   can't be copied out (the cursor then goes back to where it was, unless
///   something moved it meanwhile), `-EIO` if the filesystem broke its
///   cursor contract, and the filesystem's error from `readdir` (`-ENOENT`
///   for a `/proc/<pid>` whose process has ended). An error after some
///   entries were listed hands those on and comes back on the next call. A
///   directory removed while open lists as empty (`0`).
/// - **The directory changing between calls:** the filesystems' cursors are
///   stable as entries come and go (`VfsInode::readdir`'s contract), so a
///   listing lists every entry that stayed in the directory exactly once,
///   however many others were removed or added between calls: busybox's
///   `rm -r` (read, unlink, read on, no rewind) empties a directory. An
///   entry added or removed meanwhile may or may not be listed (POSIX
///   leaves that open). The one exception is an insert into a hashed
///   (htree) ext4 directory that splits a block (see ext4's `readdir`).
///
/// The cursor's lock is held while the entries are listed and packed in a
/// kernel buffer, and the cursor moved past them; not across the copy out,
/// which gives the position back if it fails.
pub(super) fn sys_getdents64(fd: Fd, dirp: UserPtr, count: u32) -> u64 {
    finish(getdents64(fd, dirp, count))
}

fn getdents64(fd: Fd, dirp: UserPtr, count: u32) -> Result<u64, u32> {
    let file = fd.file()?;
    file.require_non_path().map_err(VfsError::to_errno)?;
    // Here, not left to `readdir`: the trait has no default, so this is the
    // one place the rule holds for every filesystem.
    if file.inode.inode_type() != InodeType::Dir {
        return Err(errno::ENOTDIR);
    }
    let limit = (count as usize).min(GETDENTS_MAX);
    // Held across `readdir` (Lock 2 before the filesystem's Locks 4 to 9
    // and procfs's leaf `ACTIVE_PROCS`), and moved before it is released,
    // so two calls on one open file never list the same entries.
    let (pos, next, buf) = {
        let mut cursor = file.dir_cursor.lock();
        let pos = *cursor;
        let (next, buf) = pack_entries(&*file.inode, pos, limit)?;
        *cursor = next;
        (pos, next, buf)
    };
    // Not across the copy out: a refused one gives the position back, if
    // nothing moved it meanwhile.
    if !buf.is_empty()
        && let Err(e) = copy_to_user(dirp.addr(), &buf)
    {
        let mut cursor = file.dir_cursor.lock();
        if *cursor == next {
            *cursor = pos;
        }
        return Err(e.to_errno());
    }
    Ok(buf.len() as u64)
}

/// `dir`'s entries from cursor `pos`, as records within `limit` bytes: the
/// cursor past the last whole one, and the records (none at the end).
fn pack_entries(dir: &dyn VfsInode, pos: u64, limit: usize) -> Result<(u64, Vec<u8>), u32> {
    // Never more entries than could fit; at least one, so a buffer too
    // small for the next entry is told apart from the end.
    let max_entries = (limit / MIN_RECLEN).max(1);
    let mut entries = Vec::new();
    let listed = dir.readdir(pos, &mut entries, max_entries);
    pack_listed(listed, &entries, pos, limit)
}

/// What `readdir` from cursor `pos` gave (`listed`, and the `entries` it
/// pushed), as records within `limit` bytes: the cursor past the last whole
/// one, and the records.
///
/// - The entries pushed before an error are valid (`VfsInode::readdir`'s
///   contract): they are handed on, and the error, which the next call
///   meets again, comes back only when there are none.
/// - `EINVAL` if the first entry doesn't fit.
/// - The cursor contract is checked, not trusted: an entry whose
///   `next_cursor` isn't past the one before (or `pos`) would make
///   `readdir(3)` list forever. The records stop before it, or, if it is
///   the first, `EIO`.
fn pack_listed(
    listed: Result<(), VfsError>,
    entries: &[DirEntry],
    pos: u64,
    limit: usize,
) -> Result<(u64, Vec<u8>), u32> {
    if let Err(e) = listed
        && entries.is_empty()
    {
        return Err(e.to_errno());
    }
    let mut buf = Vec::new();
    let mut next = pos;
    for entry in entries {
        if entry.next_cursor <= next {
            log::warn!(
                target: "linux",
                "getdents64: readdir gave cursor {} after {next}, not past it: stopping there",
                entry.next_cursor
            );
            if buf.is_empty() {
                return Err(errno::EIO);
            }
            break;
        }
        if !push_dirent(&mut buf, entry, limit) {
            break;
        }
        next = entry.next_cursor;
    }
    if buf.is_empty() && !entries.is_empty() {
        return Err(errno::EINVAL);
    }
    Ok((next, buf))
}

/// The path [`set_cwd`] keeps: what `compute_cwd_path` gave, else
/// `fallback`'s; and the error, if it is one worth a warning (anything but
/// `ENOENT`, a directory removed by now).
fn kept_cwd_path(
    computed: Result<alloc::string::String, VfsError>,
    fallback: impl FnOnce() -> Arc<str>,
) -> (Arc<str>, Option<VfsError>) {
    match computed {
        Ok(path) => (Arc::from(path.as_str()), None),
        Err(VfsError::NoEnt) => (fallback(), None),
        Err(e) => (fallback(), Some(e)),
    }
}

/// Whether [`set_cwd`] has warned of a cwd path it couldn't compute.
static CWD_PATH_WARNED: AtomicBool = AtomicBool::new(false);

/// Make `guard`'s directory the working directory: `ENOTDIR` if it isn't a
/// directory (`Cwd::new`, checked before the path is computed). The old one
/// drops outside `ACTIVE_PROCS`.
///
/// The path kept with it (what a relative open's `/proc/<pid>/fd` path is
/// joined to) is computed from the inode, as `getcwd`'s is, so it stays
/// canonical however many relative `chdir`s led there (`chdir("..")`,
/// `chdir("sub")`, ... never make it grow). If that fails, `fallback` is
/// kept instead: quietly for a directory removed by now (`ENOENT`, also
/// what a rename storm outlasting `compute_cwd_path`'s retries says), with
/// a warning (once a boot) for anything else (`EIO`, `ENAMETOOLONG`).
fn set_cwd(guard: OpenGuard, fallback: impl FnOnce() -> Arc<str>) -> Result<u64, u32> {
    if guard.inode().inode_type() != InodeType::Dir {
        return Err(errno::ENOTDIR);
    }
    let computed = lumen_vfs::compute_cwd_path(crate::vfs::tables(), guard.inode());
    let (path, unexpected) = kept_cwd_path(computed, fallback);
    if let Some(e) = unexpected
        && !CWD_PATH_WARNED.swap(true, Ordering::Relaxed)
    {
        log::warn!(target: "linux", "chdir: the new cwd's path can't be computed ({e:?}): keeping {path:?}");
    }
    let cwd = process::Cwd::new(guard, &path).map_err(VfsError::to_errno)?;
    process::set_current_cwd(cwd);
    Ok(0)
}

/// `chdir(path)` (80): the directory `path` names (from the cwd if
/// relative, following a final symlink) becomes the working directory, so
/// relative paths and `getcwd` start there. `-ENOTDIR` if it isn't a
/// directory (or a component isn't), `-ENOENT`, `-EFAULT`, `-ENAMETOOLONG`
/// as for any path. Processes run as root, so search permission on it is
/// never refused.
///
/// The path kept for `/proc/<pid>/fd` is computed from the directory's
/// inode ([`set_cwd`]).
pub(super) fn sys_chdir(path: UserPtr) -> u64 {
    finish(chdir(path))
}

fn chdir(path: UserPtr) -> Result<u64, u32> {
    let path = UserPath::copy_in(path, false)?;
    let start = start_dir(Dirfd::Cwd, &path)?;
    let flags = ResolveFlags { follow_final: true, empty_path: false, require_dir: true };
    let resolved = start.resolve(&path, flags)?;
    // Found by a path lookup: `try_new` refuses an inode an `rmdir` freed
    // since (`ENOENT`).
    let guard = OpenGuard::try_new(resolved.inode).map_err(VfsError::to_errno)?;
    // If its path can't be computed: the path as given, joined to the old
    // cwd's if relative.
    let text = path.as_str()?;
    set_cwd(guard, || start.display_path(text).unwrap_or_else(|| Arc::from(text)))
}

/// `fchdir(fd)` (81): the directory open on `fd` (plain, `O_DIRECTORY` or
/// `O_PATH`) becomes the working directory. `-EBADF` if `fd` isn't open,
/// `-ENOTDIR` if it isn't a directory. A directory removed while open, or a
/// `/proc/<pid>` whose process has ended, is accepted (as on Linux), and
/// `getcwd` then says `-ENOENT`.
pub(super) fn sys_fchdir(fd: Fd) -> u64 {
    finish(fchdir(fd))
}

fn fchdir(fd: Fd) -> Result<u64, u32> {
    let file = fd.file()?;
    // Another reference on the open file's own: never refused, where a
    // fresh `try_new` would be on an ended process's `/proc/<pid>`.
    let guard = file.open_guard().clone();
    set_cwd(guard, || Arc::clone(&file.path))
}

/// A record `getdents64` filled, read back from the test process's memory.
#[cfg(feature = "ktest")]
struct KtestDirent {
    ino: u64,
    off: i64,
    ty: u8,
    name: alloc::string::String,
}

/// Where the ktests' `getdents64` records go in the test process's user
/// memory (up to 8 KiB): past the path slots `ktest_user_path` fills.
#[cfg(feature = "ktest")]
const KTEST_DIRENTS: u64 = process::KTEST_USER_VA + 4096;

/// Where the ktests' `getcwd` writes, in the last page.
#[cfg(feature = "ktest")]
const KTEST_CWD: u64 = process::KTEST_USER_VA + 3 * 4096;

/// The records in the `n` bytes `getdents64` filled at [`KTEST_DIRENTS`],
/// each checked: `d_reclen` a multiple of 8 within the fill and no longer
/// than its name needs, the name non-empty, NUL-terminated and followed only
/// by zeros; the records cover the `n` bytes exactly.
#[cfg(feature = "ktest")]
fn ktest_records(n: u64) -> Result<Vec<KtestDirent>, &'static str> {
    let mut bytes = alloc::vec![0u8; n as usize];
    crate::user::copy_from_user(&mut bytes, KTEST_DIRENTS).map_err(|_| "copy the records out")?;
    let field = |rec: &[u8], at: usize| <[u8; 8]>::try_from(&rec[at..at + 8]).map_err(|_| "a field");
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        if rest.len() < MIN_RECLEN {
            return Err("a record shorter than 24 bytes");
        }
        let reclen = usize::from(u16::from_le_bytes([rest[16], rest[17]]));
        if reclen < MIN_RECLEN || !reclen.is_multiple_of(8) || reclen > rest.len() {
            return Err("a d_reclen that isn't a multiple of 8 within the fill");
        }
        let rec = &rest[..reclen];
        let name_area = &rec[DIRENT_HEADER..];
        let nul = name_area.iter().position(|&b| b == 0).ok_or("a name without its NUL")?;
        if nul == 0 {
            return Err("an empty name");
        }
        if name_area[nul..].iter().any(|&b| b != 0) {
            return Err("padding after a name's NUL that isn't zero");
        }
        if dirent_reclen(nul) != reclen {
            return Err("a record longer than its name needs");
        }
        let name = core::str::from_utf8(&name_area[..nul]).map_err(|_| "a name that isn't UTF-8")?;
        out.push(KtestDirent {
            ino: u64::from_le_bytes(field(rec, 0)?),
            off: i64::from_le_bytes(field(rec, 8)?),
            ty: rec[18],
            name: name.into(),
        });
        at += reclen;
    }
    Ok(out)
}

/// `getdents64(fd, KTEST_DIRENTS, count)`.
#[cfg(feature = "ktest")]
fn ktest_getdents(fd: Fd, count: u32) -> u64 {
    sys_getdents64(fd, super::at::reg(KTEST_DIRENTS), count)
}

/// Every record left in `fd`'s directory, `count` bytes a call, until a call
/// returns 0: each call's fill at most `count` and checked by
/// [`ktest_records`], and `d_off` growing from record to record.
#[cfg(feature = "ktest")]
fn ktest_list(fd: Fd, count: u32) -> Result<Vec<KtestDirent>, &'static str> {
    let mut all: Vec<KtestDirent> = Vec::new();
    for _ in 0..1000 {
        let ret = ktest_getdents(fd, count);
        if (ret as i64) < 0 {
            crate::ktest::diag(format_args!("getdents64 returned {}", ret as i64));
            return Err("getdents64 failed");
        }
        if ret == 0 {
            return Ok(all);
        }
        if ret > u64::from(count) {
            return Err("getdents64 filled more than count");
        }
        for rec in ktest_records(ret)? {
            if all.last().is_some_and(|last| rec.off <= last.off) {
                return Err("d_off didn't grow");
            }
            all.push(rec);
        }
    }
    Err("getdents64 never reached the end")
}

/// The type of the record named `name` in `recs`, if one is.
#[cfg(feature = "ktest")]
fn ktest_type_of(recs: &[KtestDirent], name: &str) -> Option<u32> {
    recs.iter().find(|r| r.name == name).map(|r| u32::from(r.ty))
}

/// Whether no two of `recs` share a name.
#[cfg(feature = "ktest")]
fn ktest_unique(recs: &[KtestDirent]) -> bool {
    let names: alloc::collections::BTreeSet<&str> = recs.iter().map(|r| r.name.as_str()).collect();
    names.len() == recs.len()
}

/// A handler's return value as the fd it opened.
#[cfg(feature = "ktest")]
fn ktest_fd(ret: u64) -> Result<Fd, &'static str> {
    if (ret as i64) < 0 {
        crate::ktest::diag(format_args!("open returned {}", ret as i64));
        return Err("open failed");
    }
    Ok(super::at::reg(ret))
}

/// `open(path, flags)` through the handler, `path` in slot `slot`.
#[cfg(feature = "ktest")]
fn ktest_open(slot: u64, path: &[u8], flags: u32) -> Result<Fd, &'static str> {
    let path = super::fd::ktest_user_path(slot, path)?;
    ktest_fd(super::fd::sys_open(path, super::at::reg(u64::from(flags)), super::at::reg(0)))
}

/// `chdir(path)` through the handler, `path` in slot 3.
#[cfg(feature = "ktest")]
fn ktest_chdir(path: &[u8]) -> Result<u64, &'static str> {
    Ok(sys_chdir(super::fd::ktest_user_path(3, path)?))
}

/// The working directory as `getcwd` gives it.
#[cfg(feature = "ktest")]
fn ktest_cwd() -> Result<alloc::string::String, &'static str> {
    let ret = sys_getcwd(super::at::reg(KTEST_CWD), 256);
    if ret == 0 || (ret as i64) < 0 {
        return Err("getcwd failed");
    }
    let mut bytes = alloc::vec![0u8; ret as usize];
    crate::user::copy_from_user(&mut bytes, KTEST_CWD).map_err(|_| "copy the cwd out")?;
    if bytes.pop() != Some(0) {
        return Err("getcwd's path without its NUL");
    }
    alloc::string::String::from_utf8(bytes).map_err(|_| "getcwd's path isn't UTF-8")
}

/// Create the file `path` (from `/`) holding `data`.
#[cfg(feature = "ktest")]
fn ktest_file(path: &str, data: &[u8]) -> Result<(), &'static str> {
    use lumen_vfs::OpenFlags;
    let file = process::open_kernel_file(path.as_bytes(), OpenFlags::O_WRONLY | OpenFlags::O_CREAT, 0o644)
        .map_err(|_| "create a file")?;
    if !data.is_empty() && file.inode.write_at(0, data) != Ok(data.len()) {
        return Err("write a file");
    }
    Ok(())
}

/// Remove `path` (from `/`), a directory if `dir`.
#[cfg(feature = "ktest")]
fn ktest_remove(path: &str, dir: bool) -> Result<(), &'static str> {
    lumen_vfs::unlink_at(crate::vfs::tables(), None, path.as_bytes(), dir).map_err(|_| "remove a test entry")
}

/// Make the directory `path` (from `/`).
#[cfg(feature = "ktest")]
fn ktest_mkdir(path: &str) -> Result<(), &'static str> {
    lumen_vfs::mkdir_at(crate::vfs::tables(), None, path.as_bytes(), 0o755, 0).map(drop).map_err(|_| "mkdir")
}

crate::ktest!(
    fn dir_getdents64_lists_a_tmpfs_directory_exactly_once_across_small_buffers() {
        use crate::{kassert, kassert_eq};
        use alloc::format;
        use alloc::string::String;
        use lumen_vfs::OpenFlags;

        process::in_test_process(|_| {
            const DIR: &str = "/tmp/dir-ktest-many";
            ktest_mkdir(DIR)?;
            // Names of 1 to 25 bytes: records of 24, 32, 40 and 48 bytes,
            // across every 8-byte step.
            let mut expected: Vec<(String, u32)> =
                alloc::vec![(".".into(), general::DT_DIR), ("..".into(), general::DT_DIR)];
            for i in 0..40usize {
                let name = format!("{i}{}", "x".repeat(i % 24));
                ktest_file(&format!("{DIR}/{name}"), b"")?;
                expected.push((name, general::DT_REG));
            }
            ktest_mkdir(&format!("{DIR}/sub"))?;
            expected.push(("sub".into(), general::DT_DIR));
            let tables = crate::vfs::tables();
            lumen_vfs::symlink_at(tables, b"sub", None, format!("{DIR}/link").as_bytes()).map_err(|_| "symlink")?;
            expected.push(("link".into(), general::DT_LNK));
            expected.sort();
            let dir_ino = crate::vfs::resolve_path_from_root(DIR).map_err(|_| "resolve the dir")?.ino();
            let tmp_ino = crate::vfs::resolve_path_from_root("/tmp").map_err(|_| "resolve /tmp")?.ino();

            // 64 bytes a call: one or two records each, every record whole.
            let fd = ktest_open(0, DIR.as_bytes(), OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
            let got = ktest_list(fd, 64)?;
            kassert!(ktest_unique(&got));
            let mut seen: Vec<(String, u32)> = got.iter().map(|r| (r.name.clone(), u32::from(r.ty))).collect();
            seen.sort();
            kassert!(seen == expected);
            kassert_eq!(got[0].name.as_str(), ".");
            kassert_eq!(got[0].ino, dir_ino);
            kassert_eq!(got[1].name.as_str(), "..");
            kassert_eq!(got[1].ino, tmp_ino);
            // At the end, it stays there.
            kassert_eq!(ktest_getdents(fd, 4096), 0);

            // lseek(fd, 0, SEEK_SET) rewinds: the same listing, in one call.
            kassert_eq!(super::fd::sys_lseek(fd, 0, general::SEEK_SET), 0);
            let again = ktest_records(ktest_getdents(fd, 4096))?;
            kassert!(again.iter().map(|r| &r.name).eq(got.iter().map(|r| &r.name)));
            kassert_eq!(ktest_getdents(fd, 4096), 0);
            // seekdir(telldir()): lseek to a record's d_off resumes with
            // the record after it.
            let off = got[4].off as u64;
            kassert_eq!(super::fd::sys_lseek(fd, off, general::SEEK_SET), off);
            let next = ktest_records(ktest_getdents(fd, 64))?;
            kassert_eq!(next.first().map(|r| r.name.as_str()), Some(got[5].name.as_str()));

            kassert_eq!(super::fd::sys_close(fd), 0);
            for (name, ty) in &expected {
                if name != "." && name != ".." {
                    ktest_remove(&format!("{DIR}/{name}"), *ty == general::DT_DIR)?;
                }
            }
            ktest_remove(DIR, true)
        })?
    }
);

crate::ktest!(
    fn dir_getdents64_types_on_proc_dev_and_ext4() {
        use crate::{kassert, kassert_eq};
        use alloc::format;
        use lumen_vfs::{FileSystem, OpenFlags};

        process::in_test_process(|pid| {
            let dir = OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY;
            // `/proc`: its files, the `self` link and this process's
            // directory (not the exact set: other pids come and go, and
            // between calls one going would shift the cursor's index, so
            // in one call).
            let proc_fd = ktest_open(0, b"/proc", dir)?;
            let procs = ktest_list(proc_fd, 4096)?;
            kassert!(ktest_unique(&procs));
            kassert_eq!(ktest_type_of(&procs, "."), Some(general::DT_DIR));
            kassert_eq!(ktest_type_of(&procs, ".."), Some(general::DT_DIR));
            kassert_eq!(ktest_type_of(&procs, "cpuinfo"), Some(general::DT_REG));
            kassert_eq!(ktest_type_of(&procs, "self"), Some(general::DT_LNK));
            kassert_eq!(ktest_type_of(&procs, &format!("{pid}")), Some(general::DT_DIR));
            // `/proc/<pid>/fd`: this process's descriptors, as links.
            let fds_fd = ktest_open(1, format!("/proc/{pid}/fd").as_bytes(), dir)?;
            let fds = ktest_list(fds_fd, 64)?;
            kassert_eq!(ktest_type_of(&fds, "0"), Some(general::DT_LNK));
            kassert_eq!(ktest_type_of(&fds, &format!("{}", fds_fd.raw())), Some(general::DT_LNK));
            // `/dev`: character devices.
            let dev_fd = ktest_open(2, b"/dev", dir)?;
            let devs = ktest_list(dev_fd, 64)?;
            kassert!(ktest_unique(&devs));
            kassert_eq!(ktest_type_of(&devs, "null"), Some(general::DT_CHR));
            kassert_eq!(ktest_type_of(&devs, "zero"), Some(general::DT_CHR));

            // ext4: a fresh volume (not mounted), its root opened directly.
            let efs = crate::vfs::ktest_ext4(252)?;
            let root = efs.root_inode();
            root.create("file", 0o644).map_err(|_| "ext4 create")?;
            root.mkdir("sub", 0o755).map_err(|_| "ext4 mkdir")?;
            root.symlink("link", "file").map_err(|_| "ext4 symlink")?;
            // Names of 2 to 9 bytes: more than one call's worth, within the
            // test volume's one directory block.
            for i in 0..12usize {
                root.create(&format!("e{i}{}", "y".repeat(i % 8)), 0o644).map_err(|_| "ext4 create")?;
            }
            let mut truth = Vec::new();
            root.readdir(0, &mut truth, 10_000).map_err(|_| "ext4 readdir")?;
            kassert!(truth.len() >= 17);
            let file = lumen_vfs::OpenFile::new_unchecked_flags(
                alloc::sync::Arc::clone(&root),
                alloc::sync::Arc::from("/ktest-ext4"),
                OpenFlags::from_bits(dir),
            )
            .map_err(|_| "open the ext4 root")?;
            let fd = process::install_current_fd(file, false).map_err(|_| "install the ext4 root")?;
            let fd: Fd = super::at::reg(fd as u64);
            // 80 bytes a call: the listing whole, once, in the volume's order.
            let got = ktest_list(fd, 80)?;
            kassert!(ktest_unique(&got));
            kassert!(got.iter().map(|r| (r.name.as_str(), r.ino)).eq(truth.iter().map(|e| (e.name.as_str(), e.ino))));
            kassert_eq!(ktest_type_of(&got, "."), Some(general::DT_DIR));
            kassert_eq!(ktest_type_of(&got, ".."), Some(general::DT_DIR));
            kassert_eq!(ktest_type_of(&got, "file"), Some(general::DT_REG));
            kassert_eq!(ktest_type_of(&got, "sub"), Some(general::DT_DIR));
            kassert_eq!(ktest_type_of(&got, "link"), Some(general::DT_LNK));
            kassert_eq!(super::fd::sys_close(fd), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn dir_getdents64_refuses_bad_fds_buffers_and_small_counts() {
        use crate::kassert_eq;
        use alloc::format;
        use lumen_vfs::OpenFlags;

        let neg = super::neg_errno;
        process::in_test_process(move |_| {
            const DIR: &str = "/tmp/dir-ktest-errs";
            // In tmpfs's order: ".", "..", then a 20-byte name (a 40-byte
            // record), then "plain-file" (32).
            const LONG: &str = "a-twenty-byte-name-x";
            ktest_mkdir(DIR)?;
            ktest_file(&format!("{DIR}/{LONG}"), b"")?;
            ktest_file(&format!("{DIR}/plain-file"), b"")?;
            let names = |n: u64| -> Result<Vec<alloc::string::String>, &'static str> {
                Ok(ktest_records(n)?.into_iter().map(|r| r.name).collect())
            };

            // Not open, `O_PATH` (a directory, or a file: `-EBADF` before
            // `-ENOTDIR`, as on Linux), not a directory.
            kassert_eq!(ktest_getdents(super::at::reg(99), 4096), neg(errno::EBADF));
            kassert_eq!(ktest_getdents(super::at::reg(u64::from(u32::MAX)), 4096), neg(errno::EBADF));
            let opath = ktest_open(0, DIR.as_bytes(), OpenFlags::O_PATH)?;
            kassert_eq!(ktest_getdents(opath, 4096), neg(errno::EBADF));
            let opath_file = ktest_open(2, format!("{DIR}/plain-file").as_bytes(), OpenFlags::O_PATH)?;
            kassert_eq!(ktest_getdents(opath_file, 4096), neg(errno::EBADF));
            kassert_eq!(super::fd::sys_close(opath_file), 0);
            kassert_eq!(ktest_getdents(super::at::reg(0), 4096), neg(errno::ENOTDIR));
            let file = ktest_open(1, format!("{DIR}/plain-file").as_bytes(), OpenFlags::O_RDONLY)?;
            kassert_eq!(ktest_getdents(file, 4096), neg(errno::ENOTDIR));

            let fd = ktest_open(0, DIR.as_bytes(), OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
            // Too small for even "." (24 bytes).
            kassert_eq!(ktest_getdents(fd, 0), neg(errno::EINVAL));
            kassert_eq!(ktest_getdents(fd, 23), neg(errno::EINVAL));
            // Room for one record exactly: ".", then "..".
            kassert_eq!(ktest_getdents(fd, 24), 24);
            kassert_eq!(names(24)?, ["."]);
            kassert_eq!(ktest_getdents(fd, 24), 24);
            kassert_eq!(names(24)?, [".."]);
            // A bad buffer, or one running off the mapped memory: `-EFAULT`,
            // and the position stays (here past the start).
            kassert_eq!(sys_getdents64(fd, super::at::reg(0), 4096), neg(errno::EFAULT));
            let edge = process::KTEST_USER_VA + process::KTEST_USER_LEN - 8;
            kassert_eq!(sys_getdents64(fd, super::at::reg(edge), 4096), neg(errno::EFAULT));
            // The next needs 40: `-EINVAL` in 39, and it comes back next.
            kassert_eq!(ktest_getdents(fd, 39), neg(errno::EINVAL));
            kassert_eq!(ktest_getdents(fd, 40), 40);
            kassert_eq!(names(40)?, [LONG]);
            // A partial fill stops before the record that doesn't fit, which
            // the next call starts with.
            kassert_eq!(super::fd::sys_lseek(fd, 0, general::SEEK_SET), 0);
            kassert_eq!(ktest_getdents(fd, 87), 48);
            kassert_eq!(names(48)?, [".", ".."]);
            kassert_eq!(ktest_getdents(fd, 4096), 72);
            kassert_eq!(names(72)?, [LONG, "plain-file"]);
            kassert_eq!(ktest_getdents(fd, 4096), 0);

            for f in [opath, file, fd] {
                kassert_eq!(super::fd::sys_close(f), 0);
            }
            ktest_remove(&format!("{DIR}/{LONG}"), false)?;
            ktest_remove(&format!("{DIR}/plain-file"), false)?;
            ktest_remove(DIR, true)
        })?
    }
);

crate::ktest!(
    fn dir_getdents64_ends_while_entries_come_and_go() {
        // Entries removed and added between calls may shift the cursor's
        // index (a repeat or a skip), but a listing still ends, and lists
        // only names that existed; `rm -r`'s loop (list, remove, rewind)
        // empties the directory.
        use crate::{kassert, kassert_eq};
        use alloc::format;
        use lumen_vfs::OpenFlags;

        process::in_test_process(|_| {
            const DIR: &str = "/tmp/dir-ktest-churn";
            ktest_mkdir(DIR)?;
            for i in 0..30 {
                ktest_file(&format!("{DIR}/c{i:02}"), b"")?;
            }
            let fd = ktest_open(0, DIR.as_bytes(), OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
            // After each small read: the first file it listed removed, and
            // a new one added (40 at most).
            let mut added = 0;
            let mut calls = 0;
            loop {
                calls += 1;
                kassert!(calls < 500);
                let ret = ktest_getdents(fd, 48);
                kassert!((ret as i64) >= 0);
                if ret == 0 {
                    break;
                }
                let recs = ktest_records(ret)?;
                for r in &recs {
                    kassert!(r.name == "." || r.name == ".." || r.name.starts_with('c') || r.name.starts_with('n'));
                }
                if let Some(r) = recs.iter().find(|r| u32::from(r.ty) == general::DT_REG) {
                    ktest_remove(&format!("{DIR}/{}", r.name), false)?;
                }
                if added < 40 {
                    ktest_file(&format!("{DIR}/n{added:02}"), b"")?;
                    added += 1;
                }
            }
            // `rm -r`: list it all, remove what was listed, rewind; until
            // only "." and ".." are left.
            let mut passes = 0;
            loop {
                passes += 1;
                kassert!(passes < 20);
                kassert_eq!(super::fd::sys_lseek(fd, 0, general::SEEK_SET), 0);
                let recs = ktest_list(fd, 64)?;
                let files: Vec<&str> =
                    recs.iter().filter(|r| r.name != "." && r.name != "..").map(|r| r.name.as_str()).collect();
                if files.is_empty() {
                    kassert_eq!(recs.len(), 2);
                    break;
                }
                for name in files {
                    ktest_remove(&format!("{DIR}/{name}"), false)?;
                }
            }
            kassert_eq!(super::fd::sys_close(fd), 0);
            ktest_remove(DIR, true)
        })?
    }
);

crate::ktest!(
    fn dir_chdir_then_a_relative_open_and_getcwd() {
        use crate::kassert_eq;
        use lumen_vfs::OpenFlags;

        let neg = super::neg_errno;
        process::in_test_process(move |_| {
            ktest_mkdir("/tmp/dir-ktest-cd")?;
            ktest_mkdir("/tmp/dir-ktest-cd/sub")?;
            ktest_file("/tmp/dir-ktest-cd/sub/f", b"hello")?;
            let tables = crate::vfs::tables();
            lumen_vfs::symlink_at(tables, b"sub", None, b"/tmp/dir-ktest-cd/ln").map_err(|_| "symlink")?;

            kassert_eq!(ktest_chdir(b"/tmp/dir-ktest-cd")?, 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/tmp/dir-ktest-cd");
            // Relative to the new cwd.
            kassert_eq!(ktest_chdir(b"sub")?, 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/tmp/dir-ktest-cd/sub");
            // A relative open starts there too, and its `/proc` path is the
            // cwd's joined to it.
            let fd = ktest_open(0, b"f", OpenFlags::O_RDONLY)?;
            kassert_eq!(super::fd::sys_read(fd, super::at::reg(KTEST_DIRENTS), 16), 5);
            let mut got = [0u8; 5];
            crate::user::copy_from_user(&mut got, KTEST_DIRENTS).map_err(|_| "copy out")?;
            kassert_eq!(&got, b"hello");
            let shown = process::current_file(fd.raw()).map(|f| alloc::sync::Arc::clone(&f.path));
            kassert_eq!(shown.as_deref(), Some("/tmp/dir-ktest-cd/sub/f"));
            kassert_eq!(super::fd::sys_close(fd), 0);
            // "..", a symlink to a directory, a trailing slash.
            kassert_eq!(ktest_chdir(b"..")?, 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/tmp/dir-ktest-cd");
            kassert_eq!(ktest_chdir(b"ln")?, 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/tmp/dir-ktest-cd/sub");
            // The path kept for `/proc` is the directory's own, not the
            // link's, and relative `chdir`s back and forth never grow it.
            let kept = || process::current_cwd().1;
            kassert_eq!(&*kept(), "/tmp/dir-ktest-cd/sub");
            for _ in 0..50 {
                kassert_eq!(ktest_chdir(b"..")?, 0);
                kassert_eq!(ktest_chdir(b"./sub")?, 0);
            }
            kassert_eq!(&*kept(), "/tmp/dir-ktest-cd/sub");
            let fd = ktest_open(0, b"f", OpenFlags::O_RDONLY)?;
            let shown = process::current_file(fd.raw()).map(|f| alloc::sync::Arc::clone(&f.path));
            kassert_eq!(shown.as_deref(), Some("/tmp/dir-ktest-cd/sub/f"));
            kassert_eq!(super::fd::sys_close(fd), 0);
            kassert_eq!(ktest_chdir(b"/")?, 0);
            kassert_eq!(ktest_chdir(b"tmp/dir-ktest-cd/sub/")?, 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/tmp/dir-ktest-cd/sub");
            // Refused: a file, a path through one, nothing there, an empty
            // path, a bad pointer; the cwd stays.
            kassert_eq!(ktest_chdir(b"f")?, neg(errno::ENOTDIR));
            kassert_eq!(ktest_chdir(b"f/x")?, neg(errno::ENOTDIR));
            kassert_eq!(ktest_chdir(b"nope")?, neg(errno::ENOENT));
            kassert_eq!(ktest_chdir(b"")?, neg(errno::ENOENT));
            kassert_eq!(sys_chdir(super::at::reg(0)), neg(errno::EFAULT));
            kassert_eq!(ktest_cwd()?.as_str(), "/tmp/dir-ktest-cd/sub");

            kassert_eq!(ktest_chdir(b"/")?, 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/");
            ktest_remove("/tmp/dir-ktest-cd/sub/f", false)?;
            ktest_remove("/tmp/dir-ktest-cd/ln", false)?;
            ktest_remove("/tmp/dir-ktest-cd/sub", true)?;
            ktest_remove("/tmp/dir-ktest-cd", true)
        })?
    }
);

crate::ktest!(
    fn dir_fchdir_takes_plain_o_directory_and_o_path_fds() {
        use crate::kassert_eq;
        use lumen_vfs::OpenFlags;

        let neg = super::neg_errno;
        process::in_test_process(move |_| {
            ktest_mkdir("/tmp/dir-ktest-fcd")?;
            ktest_mkdir("/tmp/dir-ktest-fcd/sub")?;
            ktest_file("/tmp/dir-ktest-fcd/sub/g", b"")?;

            // `O_DIRECTORY`: the cwd, and relative opens start there.
            let dfd = ktest_open(0, b"/tmp/dir-ktest-fcd", OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
            kassert_eq!(sys_fchdir(dfd), 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/tmp/dir-ktest-fcd");
            let g = ktest_open(1, b"sub/g", OpenFlags::O_RDONLY)?;
            kassert_eq!(super::fd::sys_close(g), 0);
            // `O_PATH`: the same; the next relative open's path is the
            // directory's own joined to it.
            let pfd = ktest_open(0, b"/tmp/dir-ktest-fcd/sub", OpenFlags::O_PATH)?;
            kassert_eq!(sys_fchdir(pfd), 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/tmp/dir-ktest-fcd/sub");
            let g = ktest_open(1, b"g", OpenFlags::O_RDONLY)?;
            let shown = process::current_file(g.raw()).map(|f| alloc::sync::Arc::clone(&f.path));
            kassert_eq!(shown.as_deref(), Some("/tmp/dir-ktest-fcd/sub/g"));
            // A plain `O_RDONLY` directory.
            let rfd = ktest_open(2, b"/", OpenFlags::O_RDONLY)?;
            kassert_eq!(sys_fchdir(rfd), 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/");

            // Refused: not open, a file (plain or `O_PATH`), the console;
            // the cwd stays.
            kassert_eq!(sys_fchdir(super::at::reg(99)), neg(errno::EBADF));
            kassert_eq!(sys_fchdir(super::at::reg(u64::from(u32::MAX))), neg(errno::EBADF));
            kassert_eq!(sys_fchdir(g), neg(errno::ENOTDIR));
            let gpath = ktest_open(0, b"/tmp/dir-ktest-fcd/sub/g", OpenFlags::O_PATH)?;
            kassert_eq!(sys_fchdir(gpath), neg(errno::ENOTDIR));
            kassert_eq!(sys_fchdir(super::at::reg(0)), neg(errno::ENOTDIR));
            kassert_eq!(ktest_cwd()?.as_str(), "/");

            // A directory removed while open: accepted, and `getcwd` then
            // says it is gone.
            ktest_mkdir("/tmp/dir-ktest-fcd/gone")?;
            let gone = ktest_open(1, b"/tmp/dir-ktest-fcd/gone", OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
            ktest_remove("/tmp/dir-ktest-fcd/gone", true)?;
            kassert_eq!(sys_fchdir(gone), 0);
            kassert_eq!(sys_getcwd(super::at::reg(KTEST_CWD), 256), neg(errno::ENOENT));

            kassert_eq!(sys_fchdir(rfd), 0);
            for f in [dfd, pfd, rfd, g, gpath, gone] {
                kassert_eq!(super::fd::sys_close(f), 0);
            }
            ktest_remove("/tmp/dir-ktest-fcd/sub/g", false)?;
            ktest_remove("/tmp/dir-ktest-fcd/sub", true)?;
            ktest_remove("/tmp/dir-ktest-fcd", true)
        })?
    }
);

crate::ktest!(
    fn dir_handlers_take_their_registers_in_order() {
        // Through `dispatch`, each argument in its own register with
        // distinct values (garbage above an `int`'s 32 bits too), so an arm
        // passing them out of order can't pass.
        use crate::arch::syscall::SyscallFrame;
        use crate::kassert_eq;
        use lumen_vfs::OpenFlags;

        process::in_test_process(|_| {
            let call = |nr: u32, rdi: u64, rsi: u64, rdx: u64| {
                let mut frame = SyscallFrame { rax: u64::from(nr), rdi, rsi, rdx, ..SyscallFrame::default() };
                let _ = super::dispatch(&mut frame);
                frame.rax
            };
            let high = 0xdead_beef_0000_0000u64;
            ktest_mkdir("/tmp/dir-ktest-regs")?;
            // chdir(path), the other registers garbage.
            let path = super::fd::ktest_user_path(0, b"/tmp/dir-ktest-regs")?;
            kassert_eq!(call(general::__NR_chdir, path.addr(), high | 7, high | 9), 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/tmp/dir-ktest-regs");
            // fchdir(fd), with garbage above the fd's 32 bits.
            let root = ktest_open(1, b"/", OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
            let root_raw = root.raw() as u64;
            kassert_eq!(call(general::__NR_fchdir, high | root_raw, high | 5, 0), 0);
            kassert_eq!(ktest_cwd()?.as_str(), "/");
            // getdents64(fd, dirp, count): a count of 24 in the low half
            // holds just ".".
            kassert_eq!(call(general::__NR_getdents64, high | root_raw, KTEST_DIRENTS, high | 24), 24);
            kassert_eq!(ktest_records(24)?.first().map(|r| r.name.clone()).as_deref(), Some("."));
            kassert_eq!(super::fd::sys_close(root), 0);
            ktest_remove("/tmp/dir-ktest-regs", true)
        })?
    }
);

/// Open `dir` (on a volume of its own, not mounted) as a directory
/// descriptor of the current process, its path shown as `path`.
#[cfg(feature = "ktest")]
fn ktest_open_inode(dir: &Arc<dyn VfsInode>, path: &str) -> Result<Fd, &'static str> {
    use lumen_vfs::OpenFlags;
    let flags = OpenFlags::from_bits(OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY);
    let file = lumen_vfs::OpenFile::new_unchecked_flags(Arc::clone(dir), Arc::from(path), flags)
        .map_err(|_| "open a test volume's directory")?;
    let fd = process::install_current_fd(file, false).map_err(|_| "install a test volume's directory")?;
    Ok(super::at::reg(fd as u64))
}

/// How many entries the `rm -r` and survivor models start with: more than
/// a 2048-byte fill (musl's `readdir` buffer) holds, several times over.
#[cfg(feature = "ktest")]
const KTEST_CHURN_ENTRIES: usize = 300;

/// A test directory and the one file its names link to.
#[cfg(feature = "ktest")]
type KtestDirAndFile = (Arc<dyn VfsInode>, Arc<dyn VfsInode>);

/// Make directory `name` in `parent` holding [`KTEST_CHURN_ENTRIES`] names
/// (`entry-000`, ...; hard links to one file, so a small volume's few
/// inodes do): the directory and the file.
#[cfg(feature = "ktest")]
fn ktest_churn_dir(
    parent: &Arc<dyn VfsInode>,
    name: &str,
) -> Result<KtestDirAndFile, &'static str> {
    use alloc::format;
    let dir = parent.mkdir(name, 0o755).map_err(|_| "mkdir")?;
    let file = dir.create("entry-000", 0o644).map_err(|_| "create")?;
    for i in 1..KTEST_CHURN_ENTRIES {
        dir.link(&format!("entry-{i:03}"), &file).map_err(|_| "link")?;
    }
    Ok((dir, file))
}

/// Busybox `rm -r` over a fresh directory `name` in `parent`
/// (`libbb/remove_file.c`): `getdents64` 2048 bytes at a time, each entry
/// unlinked as soon as it is read, never a rewind; then `rmdir`, which
/// fails if an entry was skipped.
#[cfg(feature = "ktest")]
fn ktest_rm_r_model(parent: &Arc<dyn VfsInode>, name: &str) -> Result<(), &'static str> {
    let (dir, _file) = ktest_churn_dir(parent, name)?;
    let fd = ktest_open_inode(&dir, "/rm-r")?;
    let mut removed = 0;
    for _ in 0..100 {
        let ret = ktest_getdents(fd, 2048);
        if (ret as i64) < 0 {
            return Err("getdents64 failed");
        }
        if ret == 0 {
            break;
        }
        for r in ktest_records(ret)? {
            if r.name != "." && r.name != ".." {
                dir.unlink(&r.name).map_err(|_| "unlink a listed entry")?;
                removed += 1;
            }
        }
    }
    if super::fd::sys_close(fd) != 0 {
        return Err("close");
    }
    if removed != KTEST_CHURN_ENTRIES {
        crate::ktest::diag(format_args!("rm -r removed {removed} of {KTEST_CHURN_ENTRIES}"));
        return Err("rm -r skipped entries that stayed in the directory");
    }
    parent.rmdir(name).map_err(|_| "rmdir after rm -r's loop")
}

/// A listing of a fresh directory `name` in `parent`, 2048 bytes at a
/// time, with entries removed and added between calls (after each, the two
/// lowest-numbered not listed yet and the last one listed removed, a new
/// one added): every entry that stayed throughout is listed exactly once,
/// and nothing is listed twice.
#[cfg(feature = "ktest")]
fn ktest_survivors_model(parent: &Arc<dyn VfsInode>, name: &str) -> Result<(), &'static str> {
    use alloc::collections::BTreeSet;
    use alloc::format;
    use alloc::string::String;
    let (dir, file) = ktest_churn_dir(parent, name)?;
    let fd = ktest_open_inode(&dir, "/survivors")?;
    let mut seen: Vec<String> = Vec::new();
    let mut gone: BTreeSet<String> = BTreeSet::new();
    let mut added = 0;
    let mut ended = false;
    for _ in 0..100 {
        let ret = ktest_getdents(fd, 2048);
        if (ret as i64) < 0 {
            return Err("getdents64 failed");
        }
        if ret == 0 {
            ended = true;
            break;
        }
        seen.extend(ktest_records(ret)?.into_iter().map(|r| r.name).filter(|n| n != "." && n != ".."));
        let listed: BTreeSet<&str> = seen.iter().map(String::as_str).collect();
        let ahead: Vec<String> = (0..KTEST_CHURN_ENTRIES)
            .map(|i| format!("entry-{i:03}"))
            .filter(|n| !listed.contains(n.as_str()) && !gone.contains(n))
            .take(2)
            .collect();
        let behind = seen.iter().rev().find(|n| n.starts_with("entry-") && !gone.contains(*n)).cloned();
        for victim in ahead.into_iter().chain(behind) {
            dir.unlink(&victim).map_err(|_| "unlink between calls")?;
            gone.insert(victim);
        }
        if added < 20 {
            dir.link(&format!("new-{added:02}"), &file).map_err(|_| "link between calls")?;
            added += 1;
        }
    }
    if super::fd::sys_close(fd) != 0 || !ended {
        return Err("the listing never ended");
    }
    let unique: BTreeSet<&str> = seen.iter().map(String::as_str).collect();
    if unique.len() != seen.len() {
        return Err("an entry was listed twice");
    }
    for i in 0..KTEST_CHURN_ENTRIES {
        let n = format!("entry-{i:03}");
        if !gone.contains(&n) && !unique.contains(n.as_str()) {
            crate::ktest::diag(format_args!("{n} stayed in the directory but wasn't listed"));
            return Err("a survivor was skipped");
        }
    }
    if !seen.iter().all(|n| n.starts_with("entry-") || n.starts_with("new-")) {
        return Err("a name that never existed");
    }
    // Empty it and remove it.
    let mut rest = Vec::new();
    dir.readdir(0, &mut rest, 10_000).map_err(|_| "readdir")?;
    for e in rest.iter().filter(|e| e.name != "." && e.name != "..") {
        dir.unlink(&e.name).map_err(|_| "unlink the rest")?;
    }
    parent.rmdir(name).map_err(|_| "rmdir")
}

crate::ktest!(
    fn dir_getdents64_busybox_rm_r_empties_a_directory_on_tmpfs_and_ext4() {
        use crate::vfs::page_cache::ktest_files::TmpfsVolume;
        use lumen_vfs::FileSystem;
        process::in_test_process(|_| {
            let tmpfs = TmpfsVolume::new(258, 64 * 4096);
            ktest_rm_r_model(&tmpfs.root, "victim").inspect_err(|_| crate::ktest::diag(format_args!("on tmpfs")))?;
            let efs = crate::vfs::ktest_ext4(256)?;
            ktest_rm_r_model(&efs.root_inode(), "victim").inspect_err(|_| crate::ktest::diag(format_args!("on ext4")))
        })?
    }
);

crate::ktest!(
    fn dir_getdents64_lists_each_survivor_once_while_others_come_and_go() {
        use crate::vfs::page_cache::ktest_files::TmpfsVolume;
        use lumen_vfs::FileSystem;
        process::in_test_process(|_| {
            let tmpfs = TmpfsVolume::new(259, 64 * 4096);
            ktest_survivors_model(&tmpfs.root, "churn")
                .inspect_err(|_| crate::ktest::diag(format_args!("on tmpfs")))?;
            let efs = crate::vfs::ktest_ext4(257)?;
            ktest_survivors_model(&efs.root_inode(), "churn")
                .inspect_err(|_| crate::ktest::diag(format_args!("on ext4")))
        })?
    }
);

crate::ktest!(
    fn dir_getdents64_lists_across_a_gap_in_ext4_cursors() {
        // A name that isn't UTF-8 is skipped: the cursors after it have a
        // gap, which a cursor counted as `pos + 1` per entry would miss.
        use crate::{kassert, kassert_eq};
        use lumen_vfs::FileSystem;
        process::in_test_process(|_| {
            let ro = crate::vfs::ktest_ext4_non_utf8(253)?;
            let root = ro.root_inode();
            let mut truth = Vec::new();
            root.readdir(0, &mut truth, 10_000).map_err(|_| "readdir")?;
            kassert!(truth.iter().map(|e| e.name.as_str()).eq([".", "..", "a0", "a1", "a2", "a4", "a5"]));
            let fd = ktest_open_inode(&root, "/ktest-gap")?;
            // One record a call.
            let got = ktest_list(fd, 24)?;
            kassert!(ktest_unique(&got));
            kassert!(got.iter().map(|r| r.name.as_str()).eq(truth.iter().map(|e| e.name.as_str())));
            kassert!(got.iter().map(|r| r.off as u64).eq(truth.iter().map(|e| e.next_cursor)));
            kassert_eq!(super::fd::sys_close(fd), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn dir_getdents64_on_a_removed_directory_lists_nothing() {
        // Removed while open: an empty listing on tmpfs and ext4
        // (ARCHITECTURE section 9); `/proc/<pid>` of an ended process:
        // `-ENOENT`.
        use crate::kassert_eq;
        use alloc::format;
        use lumen_vfs::{FileSystem, OpenFlags};

        process::in_test_process(|_| {
            ktest_mkdir("/tmp/dir-ktest-gone")?;
            let fd = ktest_open(0, b"/tmp/dir-ktest-gone", OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
            ktest_remove("/tmp/dir-ktest-gone", true)?;
            kassert_eq!(ktest_getdents(fd, 4096), 0);
            kassert_eq!(super::fd::sys_close(fd), 0);

            let efs = crate::vfs::ktest_ext4(255)?;
            let root = efs.root_inode();
            let gone = root.mkdir("gone", 0o755).map_err(|_| "ext4 mkdir")?;
            let fd = ktest_open_inode(&gone, "/gone")?;
            root.rmdir("gone").map_err(|_| "ext4 rmdir")?;
            kassert_eq!(ktest_getdents(fd, 4096), 0);
            kassert_eq!(super::fd::sys_close(fd), 0);
            Ok::<(), &'static str>(())
        })??;

        // `/proc/<pid>` opened by its own process, listed after it ended.
        let opened = process::in_test_process(|pid| {
            let path = format!("/proc/{pid}");
            process::open_kernel_file(path.as_bytes(), OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY, 0)
                .map_err(|_| "open /proc/<pid>")
        })??;
        process::in_test_process(move |_| {
            let fd = process::install_current_fd(opened, false).map_err(|_| "install")?;
            let fd: Fd = super::at::reg(fd as u64);
            kassert_eq!(ktest_getdents(fd, 4096), super::neg_errno(errno::ENOENT));
            // `fchdir` takes it (the open file's own reference, which a
            // fresh one would be refused), and `getcwd` says it is gone.
            kassert_eq!(sys_fchdir(fd), 0);
            kassert_eq!(sys_getcwd(super::at::reg(KTEST_CWD), 256), super::neg_errno(errno::ENOENT));
            kassert_eq!(super::fd::sys_close(fd), 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn dir_fchdir_through_a_symlink_keeps_the_directory_s_own_path() {
        use crate::kassert_eq;
        use lumen_vfs::OpenFlags;
        process::in_test_process(|_| {
            ktest_mkdir("/tmp/dir-ktest-fln")?;
            ktest_mkdir("/tmp/dir-ktest-fln/sub")?;
            ktest_file("/tmp/dir-ktest-fln/sub/g", b"")?;
            let tables = crate::vfs::tables();
            lumen_vfs::symlink_at(tables, b"sub", None, b"/tmp/dir-ktest-fln/ln").map_err(|_| "symlink")?;
            // Opened through the link: the open file's path names the link,
            // the cwd's is the directory's own.
            let lfd = ktest_open(0, b"/tmp/dir-ktest-fln/ln", OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY)?;
            kassert_eq!(sys_fchdir(lfd), 0);
            kassert_eq!(&*process::current_cwd().1, "/tmp/dir-ktest-fln/sub");
            let g = ktest_open(1, b"g", OpenFlags::O_RDONLY)?;
            let shown = process::current_file(g.raw()).map(|f| Arc::clone(&f.path));
            kassert_eq!(shown.as_deref(), Some("/tmp/dir-ktest-fln/sub/g"));
            kassert_eq!(ktest_chdir(b"/")?, 0);
            for f in [lfd, g] {
                kassert_eq!(super::fd::sys_close(f), 0);
            }
            ktest_remove("/tmp/dir-ktest-fln/sub/g", false)?;
            ktest_remove("/tmp/dir-ktest-fln/ln", false)?;
            ktest_remove("/tmp/dir-ktest-fln/sub", true)?;
            ktest_remove("/tmp/dir-ktest-fln", true)
        })?
    }
);

crate::ktest!(
    fn dir_pack_listed_checks_the_cursor_contract_and_hands_on_entries_before_an_error() {
        use crate::kassert_eq;
        let e = |name: &str, next: u64| DirEntry {
            ino: 7,
            inode_type: InodeType::File,
            name: name.into(),
            next_cursor: next,
        };
        let packed = |listed, entries: &[DirEntry]| pack_listed(listed, entries, 3, 4096).map(|(n, b)| (n, b.len()));
        // A cursor that doesn't move past the one before: the records stop
        // before it; as the first, `EIO` (or `readdir(3)` would loop).
        kassert_eq!(packed(Ok(()), &[e("a", 5), e("b", 5), e("c", 9)]), Ok((5, 24)));
        kassert_eq!(packed(Ok(()), &[e("a", 6), e("b", 4)]), Ok((6, 24)));
        kassert_eq!(packed(Ok(()), &[e("a", 3)]), Err(errno::EIO));
        kassert_eq!(packed(Ok(()), &[e("a", 2)]), Err(errno::EIO));
        kassert_eq!(packed(Ok(()), &[e("a", 4), e("b", 6)]), Ok((6, 48)));
        // What `readdir` pushed before an error is handed on; the error
        // comes back alone.
        kassert_eq!(packed(Err(VfsError::Io), &[e("a", 4), e("b", 6)]), Ok((6, 48)));
        kassert_eq!(packed(Err(VfsError::Io), &[]), Err(errno::EIO));
        kassert_eq!(packed(Err(VfsError::NoEnt), &[]), Err(errno::ENOENT));
        Ok(())
    }
);

crate::ktest!(
    fn dir_kept_cwd_path_falls_back_quietly_only_for_a_removed_directory() {
        use crate::kassert_eq;
        let fallback = || Arc::<str>::from("/as/given");
        let kept = |computed| {
            let (path, warn) = kept_cwd_path(computed, fallback);
            (alloc::string::String::from(&*path), warn)
        };
        kassert_eq!(kept(Ok("/real".into())), ("/real".into(), None));
        kassert_eq!(kept(Err(VfsError::NoEnt)), ("/as/given".into(), None));
        kassert_eq!(kept(Err(VfsError::Io)), ("/as/given".into(), Some(VfsError::Io)));
        kassert_eq!(kept(Err(VfsError::NameTooLong)), ("/as/given".into(), Some(VfsError::NameTooLong)));
        Ok(())
    }
);

crate::ktest!(
    fn dir_d_type_maps_every_inode_type() {
        use crate::kassert_eq;
        let table = [
            (InodeType::File, general::DT_REG),
            (InodeType::Dir, general::DT_DIR),
            (InodeType::Symlink, general::DT_LNK),
            (InodeType::CharDevice, general::DT_CHR),
            (InodeType::BlockDevice, general::DT_BLK),
            (InodeType::Fifo, general::DT_FIFO),
            (InodeType::Socket, general::DT_SOCK),
        ];
        for (t, dt) in table {
            kassert_eq!(u32::from(d_type(t)), dt);
        }
        Ok(())
    }
);

crate::ktest!(
    fn dir_getcwd_follows_the_cwd_inode_and_fails_once_unlinked() {
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{OpenGuard, mkdir_at, rename_at2, unlink_at};

        let tables = crate::vfs::tables();
        process::in_test_process(move |_| {
            let buf = process::KTEST_USER_VA;
            let getcwd = |size: u64| sys_getcwd(super::at::reg(buf), size);
            let read_back = |len: usize| {
                let mut got = alloc::vec![0u8; len];
                crate::user::copy_from_user(&mut got, buf).map(|()| got).map_err(|_| "copy out")
            };
            // A new process's cwd: "/", two bytes with the NUL.
            kassert_eq!(getcwd(64), 2);
            kassert_eq!(read_back(2)?, b"/\0");
            kassert_eq!(getcwd(1), super::neg_errno(errno::ERANGE));
            kassert_eq!(sys_getcwd(super::at::reg(0), 64), super::neg_errno(errno::EFAULT));

            // A directory made the cwd: its path, which `size` must hold
            // with the NUL.
            let dir = mkdir_at(tables, None, b"/tmp/dir-ktest-cwd", 0o755, 0).map_err(|_| "mkdir")?;
            let guard = OpenGuard::try_new(dir).map_err(|_| "guard")?;
            process::set_current_cwd(process::Cwd::new(guard, "/tmp/dir-ktest-cwd").map_err(|_| "cwd")?);
            kassert_eq!(getcwd(64), 19);
            kassert_eq!(read_back(19)?, b"/tmp/dir-ktest-cwd\0");
            kassert_eq!(getcwd(18), super::neg_errno(errno::ERANGE));
            kassert_eq!(getcwd(19), 19);
            // Renamed: the new name, computed from the inode, not the
            // path the cwd was set by.
            let renamed = rename_at2(
                tables,
                None,
                b"/tmp/dir-ktest-cwd",
                None,
                b"/tmp/dir-ktest-moved",
                lumen_vfs::RenameFlags(0),
            );
            kassert!(renamed.is_ok());
            kassert_eq!(getcwd(64), 21);
            kassert_eq!(read_back(21)?, b"/tmp/dir-ktest-moved\0");
            // Removed while it is the cwd: `-ENOENT`, whatever the size.
            kassert!(unlink_at(tables, None, b"/tmp/dir-ktest-moved", true).is_ok());
            kassert_eq!(getcwd(64), super::neg_errno(errno::ENOENT));
            kassert_eq!(getcwd(1), super::neg_errno(errno::ENOENT));
            Ok(())
        })?
    }
);
