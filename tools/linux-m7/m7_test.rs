//! Minimal `#![no_std]` dynamically linked musl x86-64 test executable for Lumen M7/M8.
//!
//! Cross-compiled with `rustc --target x86_64-unknown-linux-musl` + `rust-lld`
//! (`-C target-feature=-crt-static`) against Alpine's `ld-musl-x86_64.so.1`,
//! using musl's `Scrt1.o` (`__libc_start_main`) to enter `main`. Every run
//! first `dlopen`s `libm7.so` (ld-musl `mmap`s it from its descriptor), checks
//! `m7_add`/`m7_abi_version` through `dlsym`, then runs its mode (#255):
//!
//! - none: `dlclose`, then `m7_test: dlopen/dlsym ok`;
//! - `shared PATH`: a `MAP_SHARED` mapping of a two-page file agrees with
//!   `pread`/`pwrite` both ways, then `msync(MS_SYNC)` (stores made through
//!   the mapping, left in the file for the host to read back);
//! - `private PATH`: a `MAP_PRIVATE` mapping's stores stay its own (the file
//!   and a shared mapping of it unchanged), and a `pwrite` to a page it
//!   copied doesn't reach it;
//! - `mprotect PATH`: through an `O_RDONLY` descriptor, a `MAP_SHARED`
//!   mapping can't be made writable (`mmap` and `mprotect` fail `EACCES`), a
//!   `MAP_PRIVATE` one can;
//! - `truncate PATH`: an `ftruncate` shrink drops the mapped page past the
//!   new end (regrown, it reads zeroes, not the old bytes), then the page is
//!   written back as it was;
//! - `sigbus PATH`: a mapped, touched page truncated away is touched again:
//!   the process dies of a bus error (`m7_test: sigbus: touching <addr> past
//!   EOF` before it names the address);
//! - `maps`: `/proc/self/maps` copied to stdout while `libm7.so` is loaded,
//!   between `m7_test: maps begin` and `m7_test: maps end`.
//!
//! A mode's success ends in `m7_test: <mode> ok` and exit 0; a failed check
//! prints `m7_test: FAIL <check>` (with `errno=<n>` after a call) and exits
//! with a code naming it.

#![no_std]
#![no_main]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

const RTLD_NOW: c_int = 2;
const O_RDONLY: c_int = 0;
const O_RDWR: c_int = 2;
const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
const MAP_PRIVATE: c_int = 2;
const MS_SYNC: c_int = 4;
const EINVAL: c_int = 22;
const EACCES: c_int = 13;
const PAGE: usize = 4096;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

// SAFETY: musl C ABI symbols resolved at runtime by `/lib/ld-musl-x86_64.so.1`
// via `libc.musl-x86_64.so.1`, declared with musl's prototypes.
unsafe extern "C" {
    fn write(fd: c_int, buf: *const u8, count: usize) -> isize;
    fn read(fd: c_int, buf: *mut u8, count: usize) -> isize;
    fn pread(fd: c_int, buf: *mut u8, count: usize, off: i64) -> isize;
    fn pwrite(fd: c_int, buf: *const u8, count: usize, off: i64) -> isize;
    fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn ftruncate(fd: c_int, len: i64) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
    fn msync(addr: *mut c_void, len: usize, flags: c_int) -> c_int;
    fn __errno_location() -> *mut c_int;
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn _exit(status: c_int) -> !;
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // SAFETY: `_exit` terminates the process immediately without unwinding.
    unsafe { _exit(127) }
}

/// The prebuilt `libcore` (built to unwind) names a personality routine in
/// its landing pads; with `panic=abort` and `_exit` on panic nothing unwinds,
/// so it is never called.
#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}

fn write_all(fd: c_int, msg: &[u8]) {
    let mut off = 0;
    while off < msg.len() {
        // SAFETY: `off < msg.len()`, so `msg.as_ptr().add(off)` stays within `msg`
        // and `msg.len() - off` bytes are valid to read.
        let n = unsafe { write(fd, msg.as_ptr().add(off), msg.len() - off) };
        if n <= 0 {
            break;
        }
        off += n as usize;
    }
}

fn errno() -> c_int {
    // SAFETY: musl's `__errno_location` returns this thread's errno slot.
    unsafe { *__errno_location() }
}

/// `n` in decimal.
fn write_num(fd: c_int, n: c_int) {
    let mut buf = [0u8; 12];
    let mut i = buf.len();
    let neg = n < 0;
    let mut v = n.unsigned_abs();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    if neg {
        i -= 1;
        buf[i] = b'-';
    }
    write_all(fd, &buf[i..]);
}

/// `n` as `0x` and lowercase hex digits.
fn write_hex(fd: c_int, n: usize) {
    let mut buf = [0u8; 18];
    let mut i = buf.len();
    let mut v = n;
    loop {
        i -= 1;
        buf[i] = b"0123456789abcdef"[v & 0xf];
        v >>= 4;
        if v == 0 {
            break;
        }
    }
    i -= 2;
    buf[i] = b'0';
    buf[i + 1] = b'x';
    write_all(fd, &buf[i..]);
}

/// Print `m7_test: FAIL <what>` and give back `code`: a check on the data.
fn fail(code: c_int, what: &[u8]) -> c_int {
    write_all(2, b"m7_test: FAIL ");
    write_all(2, what);
    write_all(2, b"\n");
    code
}

/// Print `m7_test: FAIL <what> errno=<errno>` and give back `code`: a call
/// that failed, or didn't fail as it should.
fn fail_os(code: c_int, what: &[u8]) -> c_int {
    let e = errno();
    write_all(2, b"m7_test: FAIL ");
    write_all(2, what);
    write_all(2, b" errno=");
    write_num(2, e);
    write_all(2, b"\n");
    code
}

/// A NUL-terminated copy of a C string argument (paths up to 255 bytes).
struct CPath {
    buf: [u8; 256],
}

impl CPath {
    /// # Safety
    /// `s` is a NUL-terminated string.
    unsafe fn from(s: *const u8) -> Option<CPath> {
        let mut buf = [0u8; 256];
        for (i, slot) in buf.iter_mut().enumerate() {
            // SAFETY: the caller's string is NUL-terminated and this reads up
            // to and including its NUL (or stops at 255 bytes).
            let b = unsafe { *s.add(i) };
            *slot = b;
            if b == 0 {
                return Some(CPath { buf });
            }
        }
        None
    }

    fn ptr(&self) -> *const c_char {
        self.buf.as_ptr().cast()
    }
}

fn open_path(path: &CPath, flags: c_int) -> c_int {
    // SAFETY: `path` is NUL-terminated; neither flag needs a mode argument.
    unsafe { open(path.ptr(), flags) }
}

/// `len` bytes of `fd` from `off` into `buf`, whole or not at all.
fn pread_exact(fd: c_int, buf: &mut [u8], off: i64) -> bool {
    // SAFETY: `buf` is valid for `buf.len()` bytes of writes.
    unsafe { pread(fd, buf.as_mut_ptr(), buf.len(), off) == buf.len() as isize }
}

fn pwrite_exact(fd: c_int, buf: &[u8], off: i64) -> bool {
    // SAFETY: `buf` is valid for `buf.len()` bytes of reads.
    unsafe { pwrite(fd, buf.as_ptr(), buf.len(), off) == buf.len() as isize }
}

fn map(len: usize, prot: c_int, flags: c_int, fd: c_int) -> *mut u8 {
    // SAFETY: a fresh mapping placed by the kernel (no address hint) of an
    // open descriptor; it touches no memory this program uses.
    unsafe { mmap(ptr::null_mut(), len, prot, flags, fd, 0).cast() }
}

/// The byte at `p + off` of a live mapping, read as the program would.
fn peek(p: *const u8, off: usize) -> u8 {
    // SAFETY: callers pass a live mapping and an offset within it; a fault
    // there is what the mode tests.
    unsafe { ptr::read_volatile(p.add(off)) }
}

fn poke(p: *mut u8, off: usize, b: u8) {
    // SAFETY: callers pass a live writable mapping and an offset within it.
    unsafe { ptr::write_volatile(p.add(off), b) }
}

/// Whether the mapping's first `buf.len()` bytes from `off` equal `buf`.
fn mapping_is(p: *const u8, off: usize, buf: &[u8]) -> bool {
    buf.iter().enumerate().all(|(i, &b)| peek(p, off + i) == b)
}

fn unmap(p: *mut u8, len: usize) -> bool {
    // SAFETY: `p` is a mapping of `len` bytes this program made and no longer uses.
    unsafe { munmap(p.cast(), len) == 0 }
}

/// `dlopen` `libm7.so` and check its two symbols: the handle, or the exit code.
fn load_libm7() -> Result<*mut c_void, c_int> {
    // SAFETY: `c"libm7.so"` is a static NUL-terminated string valid for `dlopen`.
    let handle = unsafe { dlopen(c"libm7.so".as_ptr(), RTLD_NOW) };
    if handle.is_null() {
        write_all(2, b"m7_test: dlopen(libm7.so) failed\n");
        return Err(1);
    }

    // SAFETY: `handle` is non-null from `dlopen` above and `c"m7_add"` is a static
    // NUL-terminated symbol name.
    let add_sym = unsafe { dlsym(handle, c"m7_add".as_ptr()) };
    if add_sym.is_null() {
        write_all(2, b"m7_test: dlsym(m7_add) failed\n");
        return Err(2);
    }

    // SAFETY: `handle` is non-null from `dlopen` above and `c"m7_abi_version"` is a
    // static NUL-terminated symbol name.
    let ver_sym = unsafe { dlsym(handle, c"m7_abi_version".as_ptr()) };
    if ver_sym.is_null() {
        write_all(2, b"m7_test: dlsym(m7_abi_version) failed\n");
        return Err(3);
    }

    // SAFETY: `add_sym` is non-null and `libm7.rs` exports `m7_add` with ABI
    // `extern "C" fn(i32, i32) -> i32`.
    let m7_add: extern "C" fn(i32, i32) -> i32 = unsafe { core::mem::transmute(add_sym) };
    // SAFETY: `ver_sym` is non-null and `libm7.rs` exports `m7_abi_version` with ABI
    // `extern "C" fn() -> u32`.
    let m7_abi_version: extern "C" fn() -> u32 = unsafe { core::mem::transmute(ver_sym) };

    if m7_abi_version() != 1 || m7_add(20, 22) != 42 {
        write_all(2, b"m7_test: unexpected libm7.so result\n");
        return Err(4);
    }
    Ok(handle)
}

/// The bytes the shared mode stores through its mapping (page 0 and page 1),
/// which the host reads back from the disk after `msync`.
const SHARED_STORE: &[u8] = b"m7 shared store via mapping\n";
const SHARED_STORE_1: &[u8] = b"m7 shared page one\n";
const SHARED_PWRITE: &[u8] = b"m7 pwrite into mapped page\n";

fn mode_shared(path: &CPath) -> c_int {
    let fd = open_path(path, O_RDWR);
    if fd < 0 {
        return fail_os(10, b"shared: open");
    }
    let p = map(2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    if p.cast() == MAP_FAILED {
        return fail_os(11, b"shared: mmap");
    }
    let mut buf = [0u8; 2 * PAGE];
    if !pread_exact(fd, &mut buf, 0) {
        return fail_os(12, b"shared: pread");
    }
    if !mapping_is(p, 0, &buf) {
        return fail(13, b"shared: mapping differs from read");
    }
    // Stores through the mapping, then read(2) sees them.
    for (i, &b) in SHARED_STORE.iter().enumerate() {
        poke(p, i, b);
    }
    for (i, &b) in SHARED_STORE_1.iter().enumerate() {
        poke(p, PAGE + i, b);
    }
    let mut got = [0u8; 64];
    if !pread_exact(fd, &mut got[..SHARED_STORE.len()], 0) || &got[..SHARED_STORE.len()] != SHARED_STORE {
        return fail(14, b"shared: read misses a store through the mapping");
    }
    if !pread_exact(fd, &mut got[..SHARED_STORE_1.len()], PAGE as i64) || &got[..SHARED_STORE_1.len()] != SHARED_STORE_1
    {
        return fail(15, b"shared: read misses a store to page 1");
    }
    // write(2), then the mapping sees it.
    if !pwrite_exact(fd, SHARED_PWRITE, 512) {
        return fail_os(16, b"shared: pwrite");
    }
    if !mapping_is(p, 512, SHARED_PWRITE) {
        return fail(17, b"shared: mapping misses a pwrite");
    }
    // SAFETY: `p` is this mode's live mapping of two pages.
    if unsafe { msync(p.cast(), 2 * PAGE, MS_SYNC) } != 0 {
        return fail_os(18, b"shared: msync(MS_SYNC)");
    }
    // SAFETY: as above; an address off a page boundary is refused.
    if unsafe { msync(p.add(1).cast(), PAGE, MS_SYNC) } == 0 || errno() != EINVAL {
        return fail_os(19, b"shared: msync of an unaligned address not EINVAL");
    }
    if !unmap(p, 2 * PAGE) {
        return fail_os(20, b"shared: munmap");
    }
    // SAFETY: `fd` is open and closed once.
    unsafe { close(fd) };
    0
}

fn mode_private(path: &CPath) -> c_int {
    let fd = open_path(path, O_RDWR);
    if fd < 0 {
        return fail_os(30, b"private: open");
    }
    let mut orig = [0u8; 2 * PAGE];
    if !pread_exact(fd, &mut orig, 0) {
        return fail_os(31, b"private: pread");
    }
    let p = map(2 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd);
    if p.cast() == MAP_FAILED {
        return fail_os(32, b"private: mmap");
    }
    let s = map(2 * PAGE, PROT_READ, MAP_SHARED, fd);
    if s.cast() == MAP_FAILED {
        return fail_os(33, b"private: shared mmap");
    }
    if !mapping_is(p, 0, &orig) {
        return fail(34, b"private: mapping differs from read");
    }
    // Copy-on-write: the store is the private mapping's alone.
    let stored = !orig[100];
    poke(p, 100, stored);
    if peek(p, 100) != stored {
        return fail(35, b"private: store lost");
    }
    let mut now = [0u8; 2 * PAGE];
    if !pread_exact(fd, &mut now, 0) || now != orig {
        return fail(36, b"private: a private store reached the file");
    }
    if !mapping_is(s, 0, &orig) {
        return fail(37, b"private: a private store reached a shared mapping");
    }
    // The file written after the copy: the copied page keeps its own bytes.
    let other = [!orig[200]];
    if !pwrite_exact(fd, &other, 200) {
        return fail_os(38, b"private: pwrite");
    }
    if peek(p, 200) != orig[200] || peek(p, 100) != stored {
        return fail(39, b"private: the copied page changed with the file");
    }
    if peek(s, 200) != other[0] {
        return fail(40, b"private: shared mapping misses a pwrite");
    }
    // Put the file back as it was.
    if !pwrite_exact(fd, &orig[200..201], 200) {
        return fail_os(41, b"private: pwrite back");
    }
    if !unmap(p, 2 * PAGE) || !unmap(s, 2 * PAGE) {
        return fail_os(42, b"private: munmap");
    }
    // SAFETY: `fd` is open and closed once.
    unsafe { close(fd) };
    0
}

fn mode_mprotect(path: &CPath) -> c_int {
    let fd = open_path(path, O_RDONLY);
    if fd < 0 {
        return fail_os(50, b"mprotect: open");
    }
    let s = map(PAGE, PROT_READ, MAP_SHARED, fd);
    if s.cast() == MAP_FAILED {
        return fail_os(51, b"mprotect: shared read-only mmap");
    }
    // SAFETY: `s` is this mode's live mapping of one page.
    if unsafe { mprotect(s.cast(), PAGE, PROT_READ | PROT_WRITE) } == 0 || errno() != EACCES {
        return fail_os(52, b"mprotect: PROT_WRITE on an O_RDONLY MAP_SHARED mapping not EACCES");
    }
    let first = peek(s, 0);
    let w = map(PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    if w.cast() != MAP_FAILED || errno() != EACCES {
        return fail_os(53, b"mprotect: writable MAP_SHARED mmap of O_RDONLY not EACCES");
    }
    // A private mapping of the same descriptor may become writable.
    let p = map(PAGE, PROT_READ, MAP_PRIVATE, fd);
    if p.cast() == MAP_FAILED {
        return fail_os(54, b"mprotect: private mmap");
    }
    // SAFETY: `p` is this mode's live mapping of one page.
    if unsafe { mprotect(p.cast(), PAGE, PROT_READ | PROT_WRITE) } != 0 {
        return fail_os(55, b"mprotect: PROT_WRITE on a MAP_PRIVATE mapping");
    }
    poke(p, 0, !first);
    if peek(p, 0) != !first || peek(s, 0) != first {
        return fail(56, b"mprotect: private store not private");
    }
    if !unmap(p, PAGE) || !unmap(s, PAGE) {
        return fail_os(57, b"mprotect: munmap");
    }
    // SAFETY: `fd` is open and closed once.
    unsafe { close(fd) };
    0
}

fn mode_truncate(path: &CPath) -> c_int {
    let fd = open_path(path, O_RDWR);
    if fd < 0 {
        return fail_os(60, b"truncate: open");
    }
    let mut orig = [0u8; 2 * PAGE];
    if !pread_exact(fd, &mut orig, 0) {
        return fail_os(61, b"truncate: pread");
    }
    if orig[PAGE..].iter().all(|&b| b == 0) {
        return fail(62, b"truncate: page 1 is all zeroes before the test");
    }
    let p = map(2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    if p.cast() == MAP_FAILED {
        return fail_os(63, b"truncate: mmap");
    }
    // Both pages mapped (their PTEs present) before the shrink.
    if !mapping_is(p, 0, &orig) {
        return fail(64, b"truncate: mapping differs from read");
    }
    // SAFETY: `fd` is an open writable descriptor.
    if unsafe { ftruncate(fd, PAGE as i64) } != 0 {
        return fail_os(65, b"truncate: ftruncate shrink");
    }
    if !mapping_is(p, 0, &orig[..PAGE]) {
        return fail(66, b"truncate: page 0 changed by the shrink");
    }
    // SAFETY: as above.
    if unsafe { ftruncate(fd, 2 * PAGE as i64) } != 0 {
        return fail_os(67, b"truncate: ftruncate regrow");
    }
    // The old page 1 went with the shrink: regrown, the file reads zeroes
    // there, through the mapping too (a stale PTE would show the old bytes).
    if (0..PAGE).any(|i| peek(p, PAGE + i) != 0) {
        return fail(68, b"truncate: mapping shows page 1's old bytes after shrink and regrow");
    }
    if !pwrite_exact(fd, &orig[PAGE..], PAGE as i64) {
        return fail_os(69, b"truncate: pwrite back");
    }
    if !mapping_is(p, PAGE, &orig[PAGE..]) {
        return fail(70, b"truncate: mapping misses the pwrite back");
    }
    if !unmap(p, 2 * PAGE) {
        return fail_os(71, b"truncate: munmap");
    }
    // SAFETY: `fd` is open and closed once.
    unsafe { close(fd) };
    0
}

fn mode_sigbus(path: &CPath) -> c_int {
    let fd = open_path(path, O_RDWR);
    if fd < 0 {
        return fail_os(80, b"sigbus: open");
    }
    let p = map(2 * PAGE, PROT_READ, MAP_SHARED, fd);
    if p.cast() == MAP_FAILED {
        return fail_os(81, b"sigbus: mmap");
    }
    let mut first = [0u8; 1];
    if !pread_exact(fd, &mut first, PAGE as i64) || peek(p, PAGE) != first[0] {
        return fail(82, b"sigbus: page 1 differs from read");
    }
    // SAFETY: `fd` is an open writable descriptor.
    if unsafe { ftruncate(fd, PAGE as i64) } != 0 {
        return fail_os(83, b"sigbus: ftruncate");
    }
    write_all(1, b"m7_test: sigbus: touching ");
    write_hex(1, p as usize + PAGE);
    write_all(1, b" past EOF\n");
    // Past the end of the file: a bus error ends the process here.
    let _ = peek(p, PAGE);
    fail(84, b"sigbus: page 1 still readable past EOF")
}

fn mode_maps() -> c_int {
    // SAFETY: a static NUL-terminated path.
    let fd = unsafe { open(c"/proc/self/maps".as_ptr(), O_RDONLY) };
    if fd < 0 {
        return fail_os(90, b"maps: open");
    }
    let mut buf = [0u8; 16384];
    let mut len = 0;
    while len < buf.len() {
        // SAFETY: `buf[len..]` is valid for its length of writes.
        let n = unsafe { read(fd, buf.as_mut_ptr().add(len), buf.len() - len) };
        if n < 0 {
            return fail_os(91, b"maps: read");
        }
        if n == 0 {
            break;
        }
        len += n as usize;
    }
    if len == buf.len() {
        return fail(92, b"maps: longer than the buffer");
    }
    // SAFETY: `fd` is open and closed once.
    unsafe { close(fd) };
    write_all(1, b"m7_test: maps begin\n");
    write_all(1, &buf[..len]);
    write_all(1, b"m7_test: maps end\n");
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    write_all(1, b"m7_test: dynamic musl start\n");

    let handle = match load_libm7() {
        Ok(handle) => handle,
        Err(code) => return code,
    };

    // SAFETY: the C runtime passes `argc` valid NUL-terminated strings.
    let arg = |i: c_int| (i < argc).then(|| unsafe { *argv.add(i as usize) });
    let Some(mode) = arg(1) else {
        // SAFETY: `handle` is a valid open `dlopen` handle not yet closed.
        if unsafe { dlclose(handle) } != 0 {
            write_all(2, b"m7_test: dlclose failed\n");
            return 5;
        }
        write_all(1, b"m7_test: dlopen/dlsym ok\n");
        return 0;
    };
    // SAFETY: `mode` is an `argv` string.
    let Some(mode) = (unsafe { CPath::from(mode) }) else {
        return fail(6, b"argument too long");
    };
    let name = &mode.buf[..mode.buf.iter().position(|&b| b == 0).unwrap_or(0)];
    let path = match arg(2) {
        // SAFETY: an `argv` string.
        Some(p) => match unsafe { CPath::from(p) } {
            Some(p) => Some(p),
            None => return fail(6, b"argument too long"),
        },
        None => None,
    };
    let rc = match (name, &path) {
        (b"shared", Some(p)) => mode_shared(p),
        (b"private", Some(p)) => mode_private(p),
        (b"mprotect", Some(p)) => mode_mprotect(p),
        (b"truncate", Some(p)) => mode_truncate(p),
        (b"sigbus", Some(p)) => mode_sigbus(p),
        (b"maps", None) => mode_maps(),
        _ => {
            write_all(2, b"usage: m7_test [shared|private|mprotect|truncate|sigbus PATH | maps]\n");
            return 7;
        }
    };
    if rc != 0 {
        return rc;
    }
    write_all(1, b"m7_test: ");
    write_all(1, name);
    write_all(1, b" ok\n");
    0
}
