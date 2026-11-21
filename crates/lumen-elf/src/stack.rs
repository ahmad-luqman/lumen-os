//! System V AMD64 / `musl` initial user stack builder.
//!
//! Lays out `argv` strings, `envp` strings, `execfn`, 16 `AT_RANDOM` bytes,
//! optional 8-byte alignment padding, `auxv`, `envp` pointers + null,
//! `argv` pointers + null, and `argc` at `[rsp]` into a caller-provided stack
//! buffer representing `[stack_top - buf.len() .. stack_top)`.
//!
//! Guarantees `%rsp % 16 == 0` at `argc` for every combination of `argc` and
//! `envc`, performs zero heap allocations, and never modifies `buf` on error.

use core::fmt;

use crate::loader::{LoadedElf, PAGE_SIZE, USER_MAX, USER_MIN};

/// Default initial user stack top (`docs/ARCHITECTURE.md` §5).
pub const USER_STACK_TOP: u64 = 0x0000_7fff_f000_0000;

/// Auxiliary vector end marker.
pub const AT_NULL: u64 = 0;
/// Program headers address in memory.
pub const AT_PHDR: u64 = 3;
/// Size of one program header entry.
pub const AT_PHENT: u64 = 4;
/// Number of program headers.
pub const AT_PHNUM: u64 = 5;
/// Architectural page size (4096).
pub const AT_PAGESZ: u64 = 6;
/// Dynamic interpreter base address (omitted or 0 for static PIE).
pub const AT_BASE: u64 = 7;
/// Entry point of the main program.
pub const AT_ENTRY: u64 = 9;
/// Real user ID.
pub const AT_UID: u64 = 11;
/// Effective user ID.
pub const AT_EUID: u64 = 12;
/// Real group ID.
pub const AT_GID: u64 = 13;
/// Effective group ID.
pub const AT_EGID: u64 = 14;
/// Frequency at which `times()` increments (100 Hz).
pub const AT_CLKTCK: u64 = 17;
/// Secure mode boolean.
pub const AT_SECURE: u64 = 23;
/// Address of 16 random bytes.
pub const AT_RANDOM: u64 = 25;
/// Address of executable filename string.
pub const AT_EXECFN: u64 = 31;

/// Default `AT_CLKTCK` frequency (100 ticks/sec, matching Linux x86-64).
pub const DEFAULT_CLKTCK: u64 = 100;

/// Metadata written into the Linux / System V auxiliary vector (`auxv`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuxvInfo {
    /// `AT_PHDR`: mapped user VA of the program header table (`0` if unmapped).
    pub phdr: u64,
    /// `AT_PHENT`: byte size of one program header entry.
    pub phent: u64,
    /// `AT_PHNUM`: number of program header entries.
    pub phnum: u64,
    /// `AT_PAGESZ`: page size (`4096`).
    pub pagesz: u64,
    /// `AT_BASE`: dynamic interpreter load base (`None` omits `AT_BASE`, as for
    /// static `ET_EXEC` and static PIE `ET_DYN`).
    pub base: Option<u64>,
    /// `AT_ENTRY`: mapped user VA of the binary's entry point.
    pub entry: u64,
    /// `AT_UID`: real UID.
    pub uid: u64,
    /// `AT_EUID`: effective UID.
    pub euid: u64,
    /// `AT_GID`: real GID.
    pub gid: u64,
    /// `AT_EGID`: effective GID.
    pub egid: u64,
    /// `AT_SECURE`: secure mode flag (`0`).
    pub secure: u64,
    /// `AT_CLKTCK`: clock ticks per second (`100`).
    pub clktck: u64,
}

impl AuxvInfo {
    /// Construct M6 default `auxv` metadata from a validated [`LoadedElf`].
    ///
    /// When the program header table is not mapped into user memory
    /// (`loaded.phdr_vaddr` is `None`), both `phdr` and `phnum` are set to `0`
    /// so `musl`'s `static_init_tls` never dereferences a null `AT_PHDR`.
    pub const fn from_loaded(loaded: &LoadedElf) -> Self {
        let (phdr, phnum) = match loaded.phdr_vaddr {
            Some(va) => (va, loaded.phnum as u64),
            None => (0, 0),
        };
        Self {
            phdr,
            phent: loaded.phent as u64,
            phnum,
            pagesz: PAGE_SIZE,
            base: None,
            entry: loaded.entry,
            uid: 0,
            euid: 0,
            gid: 0,
            egid: 0,
            secure: 0,
            clktck: DEFAULT_CLKTCK,
        }
    }

    /// Construct `auxv` metadata for a dynamically linked (`PT_INTERP`) binary,
    /// keeping `AT_PHDR`/`AT_PHENT`/`AT_PHNUM`/`AT_ENTRY` from `main` while
    /// setting `AT_BASE` to `Some(interp_load_bias)`.
    pub const fn from_loaded_with_interp(main: &LoadedElf, interp_load_bias: u64) -> Self {
        let mut info = Self::from_loaded(main);
        info.base = Some(interp_load_bias);
        info
    }
}

/// Output addresses produced by [`build_initial_stack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitialStack {
    /// Final initial `%rsp` pointing at `argc` (always 16-byte aligned).
    pub rsp: u64,
    /// Mapped user VA of the 16 random bytes referenced by `AT_RANDOM`.
    pub random_vaddr: u64,
    /// Mapped user VA of the NUL-terminated `execfn` string referenced by `AT_EXECFN`.
    pub execfn_vaddr: u64,
    /// Total bytes written at the top of the stack buffer (`stack_top - rsp`).
    pub bytes_written: usize,
}

/// Why building the initial user stack failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackBuildError {
    /// `stack_top` is not 16-byte aligned.
    UnalignedStackTop(u64),
    /// `[stack_top - buf.len() .. stack_top)` underflows or lies outside `USER_MIN..USER_MAX`.
    InvalidStackTop { stack_top: u64, buf_len: usize },
    /// One of `argv`, `envp`, or `execfn` contains an interior NUL byte.
    InteriorNul,
    /// Total stack frame size overflowed `usize`.
    SizeOverflow,
    /// Caller's stack buffer is too small for the strings, pointers, and `auxv`.
    BufferTooSmall { required: usize, available: usize },
}

impl fmt::Display for StackBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::UnalignedStackTop(top) => write!(f, "stack_top {top:#x} is not 16-byte aligned"),
            Self::InvalidStackTop { stack_top, buf_len } => {
                write!(f, "stack range for top {stack_top:#x} ({buf_len} bytes) lies outside user space")
            }
            Self::InteriorNul => f.write_str("argv/envp/execfn string contains an interior NUL byte"),
            Self::SizeOverflow => f.write_str("initial user stack size overflow"),
            Self::BufferTooSmall { required, available } => {
                write!(f, "stack buffer too small: need {required} bytes, have {available}")
            }
        }
    }
}

#[inline]
const fn checked_align_up_16(n: usize) -> Option<usize> {
    match n.checked_add(15) {
        Some(v) => Some(v & !15),
        None => None,
    }
}

/// Build the System V AMD64 / `musl` initial user stack into `buf`.
///
/// `buf` represents user virtual addresses `[stack_top - buf.len() .. stack_top)`.
///
/// Layout from high address (`stack_top`) down to `rsp`:
/// 1. `argv` NUL-terminated strings
/// 2. `envp` NUL-terminated strings
/// 3. `execfn` NUL-terminated string
/// 4. 0..15 zero bytes so `AT_RANDOM` is 16-byte aligned
/// 5. 16 random bytes (`AT_RANDOM`)
/// 6. Optional 8-byte zero pad when `1 + argc + 1 + envc + 1` is odd
/// 7. `auxv` `(key, value)` pairs ending with `(AT_NULL, 0)`
/// 8. `envp` pointer array + trailing `0`
/// 9. `argv` pointer array + trailing `0`
/// 10. `argc` at `[rsp]` (`rsp % 16 == 0`)
///
/// On error, `buf` is not modified.
pub fn build_initial_stack(
    buf: &mut [u8],
    stack_top: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    execfn: &[u8],
    random: [u8; 16],
    auxv: &AuxvInfo,
) -> Result<InitialStack, StackBuildError> {
    if (stack_top & 15) != 0 {
        return Err(StackBuildError::UnalignedStackTop(stack_top));
    }
    let stack_bottom = stack_top
        .checked_sub(buf.len() as u64)
        .filter(|&b| b >= USER_MIN && stack_top <= USER_MAX)
        .ok_or(StackBuildError::InvalidStackTop { stack_top, buf_len: buf.len() })?;

    // Validate no interior NULs and compute total string bytes with checked arithmetic.
    let mut str_bytes: usize = 0;
    for s in argv.iter().chain(envp.iter()).copied().chain(core::iter::once(execfn)) {
        if s.contains(&0) {
            return Err(StackBuildError::InteriorNul);
        }
        let with_nul = s.len().checked_add(1).ok_or(StackBuildError::SizeOverflow)?;
        str_bytes = str_bytes.checked_add(with_nul).ok_or(StackBuildError::SizeOverflow)?;
    }

    // Distance from `stack_top` down to the start of the 16-byte AT_RANDOM block.
    // Because `stack_top % 16 == 0` and `aligned_str_bytes % 16 == 0`, `random_vaddr`
    // is always 16-byte aligned.
    let aligned_str_bytes = checked_align_up_16(str_bytes).ok_or(StackBuildError::SizeOverflow)?;
    let random_top_dist = aligned_str_bytes.checked_add(16).ok_or(StackBuildError::SizeOverflow)?;

    // Words in `[argc, argv..., NULL, envp..., NULL]`: `1 + argc + 1 + envc + 1`.
    let ptr_words =
        argv.len().checked_add(envp.len()).and_then(|n| n.checked_add(3)).ok_or(StackBuildError::SizeOverflow)?;

    // Insert an 8-byte alignment pad above `auxv` whenever `1 + argc + 1 + envc + 1`
    // is odd so that `rsp` at `argc` is 16-byte aligned (auxv is always an even
    // number of u64 words).
    let pad_bytes: usize = if (ptr_words & 1) == 1 { 8 } else { 0 };

    // 14 standard entries (including AT_NULL), plus optional AT_BASE.
    let auxv_pairs: usize = if auxv.base.is_some() { 15 } else { 14 };
    let auxv_words = auxv_pairs * 2;

    let table_words = ptr_words.checked_add(auxv_words).ok_or(StackBuildError::SizeOverflow)?;
    let table_bytes = table_words.checked_mul(8).ok_or(StackBuildError::SizeOverflow)?;
    let total_bytes = random_top_dist
        .checked_add(pad_bytes)
        .and_then(|n| n.checked_add(table_bytes))
        .ok_or(StackBuildError::SizeOverflow)?;

    if total_bytes > buf.len() {
        return Err(StackBuildError::BufferTooSmall { required: total_bytes, available: buf.len() });
    }

    let frame_start = buf.len() - total_bytes;
    // Zero the entire stack frame so padding, NUL terminators, and null pointers are clean.
    buf[frame_start..].fill(0);

    let mut str_cursor = buf.len();
    let mut ptr_cursor = frame_start;

    let write_u64 = |buf: &mut [u8], cursor: &mut usize, val: u64| {
        buf[*cursor..*cursor + 8].copy_from_slice(&val.to_le_bytes());
        *cursor += 8;
    };

    // 1. argc at [rsp]
    write_u64(buf, &mut ptr_cursor, argv.len() as u64);

    // 2. argv strings (top-down) and argv pointer array (bottom-up)
    for &arg in argv {
        str_cursor -= arg.len() + 1;
        buf[str_cursor..str_cursor + arg.len()].copy_from_slice(arg);
        let va = stack_bottom + str_cursor as u64;
        write_u64(buf, &mut ptr_cursor, va);
    }
    // argv NULL terminator
    write_u64(buf, &mut ptr_cursor, 0);

    // 3. envp strings (top-down) and envp pointer array (bottom-up)
    for &env in envp {
        str_cursor -= env.len() + 1;
        buf[str_cursor..str_cursor + env.len()].copy_from_slice(env);
        let va = stack_bottom + str_cursor as u64;
        write_u64(buf, &mut ptr_cursor, va);
    }
    // envp NULL terminator
    write_u64(buf, &mut ptr_cursor, 0);

    // 4. execfn string (top-down)
    str_cursor -= execfn.len() + 1;
    buf[str_cursor..str_cursor + execfn.len()].copy_from_slice(execfn);
    let execfn_vaddr = stack_bottom + str_cursor as u64;

    // 5. 16 random bytes (16-byte aligned below all strings)
    let random_off = buf.len() - random_top_dist;
    buf[random_off..random_off + 16].copy_from_slice(&random);
    let random_vaddr = stack_bottom + random_off as u64;

    // 6. Auxiliary vector pairs
    let mut write_aux = |key: u64, val: u64| {
        write_u64(buf, &mut ptr_cursor, key);
        write_u64(buf, &mut ptr_cursor, val);
    };
    write_aux(AT_PHDR, auxv.phdr);
    write_aux(AT_PHENT, auxv.phent);
    write_aux(AT_PHNUM, auxv.phnum);
    write_aux(AT_PAGESZ, auxv.pagesz);
    if let Some(base) = auxv.base {
        write_aux(AT_BASE, base);
    }
    write_aux(AT_ENTRY, auxv.entry);
    write_aux(AT_UID, auxv.uid);
    write_aux(AT_EUID, auxv.euid);
    write_aux(AT_GID, auxv.gid);
    write_aux(AT_EGID, auxv.egid);
    write_aux(AT_SECURE, auxv.secure);
    write_aux(AT_CLKTCK, auxv.clktck);
    write_aux(AT_EXECFN, execfn_vaddr);
    write_aux(AT_RANDOM, random_vaddr);
    write_aux(AT_NULL, 0);

    debug_assert_eq!(ptr_cursor + pad_bytes, random_off);

    let rsp = stack_top - total_bytes as u64;
    debug_assert_eq!(rsp & 15, 0);

    Ok(InitialStack { rsp, random_vaddr, execfn_vaddr, bytes_written: total_bytes })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::{vec, vec::Vec};

    fn sample_auxv(base: Option<u64>) -> AuxvInfo {
        AuxvInfo {
            phdr: 0x0000_5555_5555_4040,
            phent: 56,
            phnum: 4,
            pagesz: PAGE_SIZE,
            base,
            entry: 0x0000_5555_5555_5010,
            uid: 0,
            euid: 0,
            gid: 0,
            egid: 0,
            secure: 0,
            clktck: DEFAULT_CLKTCK,
        }
    }

    fn read_u64_at(buf: &[u8], stack_bottom: u64, va: u64) -> u64 {
        let off = usize::try_from(va - stack_bottom).unwrap();
        u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
    }

    fn read_cstr_at(buf: &[u8], stack_bottom: u64, va: u64) -> Vec<u8> {
        let off = usize::try_from(va - stack_bottom).unwrap();
        let rest = &buf[off..];
        let nul = rest.iter().position(|&b| b == 0).expect("missing NUL terminator");
        rest[..nul].to_vec()
    }

    #[test]
    fn alignment_and_layout_across_all_even_odd_argc_envc_combinations() {
        let all_args: [&[u8]; 3] = [b"/bin/busybox", b"ash", b"-s"];
        let all_envs: [&[u8]; 3] = [b"PATH=/bin:/usr/bin", b"HOME=/root", b"TERM=vt100"];
        let execfn = b"/bin/busybox";
        let random = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];

        for base in [None, Some(0x0000_7f00_0000_0000)] {
            let auxv = sample_auxv(base);
            for argc in 0..=3 {
                for envc in 0..=3 {
                    let argv = &all_args[..argc];
                    let envp = &all_envs[..envc];
                    let mut buf = vec![0xCCu8; 4096];
                    let stack_top = USER_STACK_TOP;
                    let stack_bottom = stack_top - buf.len() as u64;

                    let info = build_initial_stack(&mut buf, stack_top, argv, envp, execfn, random, &auxv).unwrap();

                    // 16-byte alignment at argc AND at AT_RANDOM
                    assert_eq!(info.rsp % 16, 0, "rsp misaligned for argc={argc}, envc={envc}");
                    assert_eq!(info.random_vaddr % 16, 0);
                    assert_eq!(info.rsp, stack_top - info.bytes_written as u64);

                    // Unused bytes below rsp remain untouched
                    let frame_start = buf.len() - info.bytes_written;
                    assert!(buf[..frame_start].iter().all(|&b| b == 0xCC));

                    let mut cur = info.rsp;

                    // argc
                    assert_eq!(read_u64_at(&buf, stack_bottom, cur), argc as u64);
                    cur += 8;

                    // argv pointers + NULL
                    for &expected in argv {
                        let ptr = read_u64_at(&buf, stack_bottom, cur);
                        assert!(ptr >= stack_bottom && ptr < stack_top);
                        assert_eq!(read_cstr_at(&buf, stack_bottom, ptr), expected);
                        cur += 8;
                    }
                    assert_eq!(read_u64_at(&buf, stack_bottom, cur), 0);
                    cur += 8;

                    // envp pointers + NULL
                    for &expected in envp {
                        let ptr = read_u64_at(&buf, stack_bottom, cur);
                        assert!(ptr >= stack_bottom && ptr < stack_top);
                        assert_eq!(read_cstr_at(&buf, stack_bottom, ptr), expected);
                        cur += 8;
                    }
                    assert_eq!(read_u64_at(&buf, stack_bottom, cur), 0);
                    cur += 8;

                    // auxv pairs
                    let mut pairs = Vec::new();
                    loop {
                        let key = read_u64_at(&buf, stack_bottom, cur);
                        let val = read_u64_at(&buf, stack_bottom, cur + 8);
                        cur += 16;
                        pairs.push((key, val));
                        if key == AT_NULL {
                            assert_eq!(val, 0);
                            break;
                        }
                    }

                    let mut expected_pairs =
                        vec![(AT_PHDR, auxv.phdr), (AT_PHENT, auxv.phent), (AT_PHNUM, auxv.phnum), (AT_PAGESZ, 4096)];
                    if let Some(b) = base {
                        expected_pairs.push((AT_BASE, b));
                    }
                    expected_pairs.extend_from_slice(&[
                        (AT_ENTRY, auxv.entry),
                        (AT_UID, 0),
                        (AT_EUID, 0),
                        (AT_GID, 0),
                        (AT_EGID, 0),
                        (AT_SECURE, 0),
                        (AT_CLKTCK, 100),
                        (AT_EXECFN, info.execfn_vaddr),
                        (AT_RANDOM, info.random_vaddr),
                        (AT_NULL, 0),
                    ]);
                    assert_eq!(pairs, expected_pairs);

                    // Verify optional 8-byte pad above auxv matches parity of `1 + argc + 1 + envc + 1`
                    let odd = (1 + argc + 1 + envc + 1) % 2 == 1;
                    let expected_gap = if odd { 8u64 } else { 0u64 };
                    assert_eq!(info.random_vaddr - cur, expected_gap);
                    if odd {
                        assert_eq!(read_u64_at(&buf, stack_bottom, cur), 0);
                    }

                    // Verify AT_RANDOM bytes and AT_EXECFN string
                    let rand_off = usize::try_from(info.random_vaddr - stack_bottom).unwrap();
                    assert_eq!(&buf[rand_off..rand_off + 16], &random);
                    assert_eq!(read_cstr_at(&buf, stack_bottom, info.execfn_vaddr), execfn);
                }
            }
        }
    }

    #[test]
    fn exact_capacity_succeeds_and_one_byte_short_fails_without_mutating_buf() {
        let auxv = sample_auxv(None);
        let argv: [&[u8]; 2] = [b"prog", b"--flag"];
        let envp: [&[u8]; 1] = [b"K=V"];
        let execfn = b"/bin/prog";
        let random = [0x42u8; 16];

        // First probe required size with an empty buffer.
        let Err(StackBuildError::BufferTooSmall { required, available: 0 }) =
            build_initial_stack(&mut [], USER_STACK_TOP, &argv, &envp, execfn, random, &auxv)
        else {
            panic!("expected BufferTooSmall");
        };

        // One byte short must fail and leave buffer untouched.
        let mut short_buf = vec![0xAAu8; required - 1];
        assert_eq!(
            build_initial_stack(&mut short_buf, USER_STACK_TOP, &argv, &envp, execfn, random, &auxv),
            Err(StackBuildError::BufferTooSmall { required, available: required - 1 })
        );
        assert!(short_buf.iter().all(|&b| b == 0xAA));

        // Exact size must succeed with rsp == stack_top - required.
        let mut exact_buf = vec![0xAAu8; required];
        let info = build_initial_stack(&mut exact_buf, USER_STACK_TOP, &argv, &envp, execfn, random, &auxv).unwrap();
        assert_eq!(info.bytes_written, required);
        assert_eq!(info.rsp, USER_STACK_TOP - required as u64);
        assert_eq!(info.rsp % 16, 0);
    }

    #[test]
    fn rejects_invalid_inputs_without_panicking() {
        let auxv = sample_auxv(None);
        let mut buf = [0u8; 1024];

        // Misaligned stack_top
        assert_eq!(
            build_initial_stack(&mut buf, USER_STACK_TOP + 8, &[], &[], b"x", [0; 16], &auxv),
            Err(StackBuildError::UnalignedStackTop(USER_STACK_TOP + 8))
        );

        // Underflowing or below USER_MIN / above USER_MAX
        assert_eq!(
            build_initial_stack(&mut buf, 1024, &[], &[], b"x", [0; 16], &auxv),
            Err(StackBuildError::InvalidStackTop { stack_top: 1024, buf_len: 1024 })
        );
        assert_eq!(
            build_initial_stack(&mut buf, 512, &[], &[], b"x", [0; 16], &auxv),
            Err(StackBuildError::InvalidStackTop { stack_top: 512, buf_len: 1024 })
        );
        assert_eq!(
            build_initial_stack(&mut buf, USER_MIN + 512, &[], &[], b"x", [0; 16], &auxv),
            Err(StackBuildError::InvalidStackTop { stack_top: USER_MIN + 512, buf_len: 1024 })
        );
        assert_eq!(
            build_initial_stack(&mut buf, USER_MAX + 16, &[], &[], b"x", [0; 16], &auxv),
            Err(StackBuildError::InvalidStackTop { stack_top: USER_MAX + 16, buf_len: 1024 })
        );
        // Exact USER_MIN bottom and exact USER_MAX top succeed.
        assert!(build_initial_stack(&mut buf, USER_MIN + 1024, &[], &[], b"x", [0; 16], &auxv).is_ok());
        assert!(build_initial_stack(&mut buf, USER_MAX, &[], &[], b"x", [0; 16], &auxv).is_ok());

        // Interior NUL in argv, envp, and execfn
        assert_eq!(
            build_initial_stack(&mut buf, USER_STACK_TOP, &[b"a\0b"], &[], b"x", [0; 16], &auxv),
            Err(StackBuildError::InteriorNul)
        );
        assert_eq!(
            build_initial_stack(&mut buf, USER_STACK_TOP, &[], &[b"K=V\0"], b"x", [0; 16], &auxv),
            Err(StackBuildError::InteriorNul)
        );
        assert_eq!(
            build_initial_stack(&mut buf, USER_STACK_TOP, &[], &[], b"/bin/\0x", [0; 16], &auxv),
            Err(StackBuildError::InteriorNul)
        );
    }

    #[test]
    fn empty_strings_are_valid() {
        let auxv = sample_auxv(None);
        let mut buf = vec![0u8; 1024];
        let stack_bottom = USER_STACK_TOP - buf.len() as u64;
        let info = build_initial_stack(&mut buf, USER_STACK_TOP, &[b""], &[b""], b"", [7; 16], &auxv).unwrap();
        assert_eq!(info.rsp % 16, 0);
        let argv0 = read_u64_at(&buf, stack_bottom, info.rsp + 8);
        assert_eq!(read_cstr_at(&buf, stack_bottom, argv0), b"");
        let envp0 = read_u64_at(&buf, stack_bottom, info.rsp + 24);
        assert_eq!(read_cstr_at(&buf, stack_bottom, envp0), b"");
        assert_eq!(read_cstr_at(&buf, stack_bottom, info.execfn_vaddr), b"");
    }

    #[test]
    fn auxv_from_loaded_static_pie_omits_at_base() {
        use crate::loader::{EM_X86_64, ET_DYN, EV_CURRENT, PF_R, PF_X, PHDR_MIN_SIZE, PIE_BASE, PT_LOAD};

        // Build a minimal static PIE ELF64 image with headers mapped at p_vaddr = 0.
        let mut img = vec![0u8; 0x200];
        img[0..4].copy_from_slice(&crate::ELF_MAGIC);
        img[4] = crate::ELFCLASS64;
        img[5] = crate::ELFDATA2LSB;
        img[6] = EV_CURRENT;
        img[0x10..0x12].copy_from_slice(&ET_DYN.to_le_bytes());
        img[0x12..0x14].copy_from_slice(&EM_X86_64.to_le_bytes());
        img[0x14..0x18].copy_from_slice(&u32::from(EV_CURRENT).to_le_bytes());
        img[0x18..0x20].copy_from_slice(&0x100u64.to_le_bytes());
        img[0x20..0x28].copy_from_slice(&(crate::EHDR_SIZE as u64).to_le_bytes());
        img[0x36..0x38].copy_from_slice(&PHDR_MIN_SIZE.to_le_bytes());
        img[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());

        // Single R-X PT_LOAD covering the ELF header, phdr table, and entry at 0x100.
        let o = crate::EHDR_SIZE;
        img[o..o + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        img[o + 4..o + 8].copy_from_slice(&(PF_R | PF_X).to_le_bytes());
        img[o + 8..o + 16].copy_from_slice(&0u64.to_le_bytes());
        img[o + 16..o + 24].copy_from_slice(&0u64.to_le_bytes());
        img[o + 24..o + 32].copy_from_slice(&0u64.to_le_bytes());
        img[o + 32..o + 40].copy_from_slice(&0x200u64.to_le_bytes());
        img[o + 40..o + 48].copy_from_slice(&0x200u64.to_le_bytes());
        img[o + 48..o + 56].copy_from_slice(&PAGE_SIZE.to_le_bytes());

        let loaded = LoadedElf::parse(&img).unwrap();
        let auxv = AuxvInfo::from_loaded(&loaded);
        assert_eq!(auxv.base, None);
        assert_eq!(auxv.phdr, PIE_BASE + 64);
        assert_eq!(auxv.phent, 56);
        assert_eq!(auxv.phnum, 1);
        assert_eq!(auxv.entry, PIE_BASE + 0x100);
        assert_eq!(auxv.pagesz, 4096);
        assert_eq!(auxv.clktck, 100);
        assert_eq!(auxv.secure, 0);

        let mut buf = vec![0u8; 1024];
        let stack_bottom = USER_STACK_TOP - buf.len() as u64;
        let info = build_initial_stack(&mut buf, USER_STACK_TOP, &[b"/hello"], &[], b"/hello", [9; 16], &auxv).unwrap();
        assert_eq!(info.rsp % 16, 0);

        // Scan auxv and verify AT_BASE is omitted while AT_PHDR and AT_ENTRY match.
        let mut cur = info.rsp + 4 * 8; // argc + argv[0] + NULL + envp NULL
        let mut saw_base = false;
        let mut saw_phdr = false;
        let mut saw_entry = false;
        loop {
            let k = read_u64_at(&buf, stack_bottom, cur);
            let v = read_u64_at(&buf, stack_bottom, cur + 8);
            cur += 16;
            if k == AT_BASE {
                saw_base = true;
            }
            if k == AT_PHDR {
                assert_eq!(v, PIE_BASE + 64);
                saw_phdr = true;
            }
            if k == AT_ENTRY {
                assert_eq!(v, PIE_BASE + 0x100);
                saw_entry = true;
            }
            if k == AT_NULL {
                break;
            }
        }
        assert!(!saw_base);
        assert!(saw_phdr);
        assert!(saw_entry);
    }

    #[test]
    fn auxv_from_loaded_zeros_phnum_when_phdrs_unmapped() {
        use crate::loader::{EM_X86_64, ET_EXEC, EV_CURRENT, PF_R, PF_X, PHDR_MIN_SIZE, PT_LOAD};

        // Minimal ET_EXEC whose only PT_LOAD starts at file offset 0x100 (after
        // the ELF header and phdr table), so phdr_vaddr is None.
        let mut img = vec![0u8; 0x200];
        img[0..4].copy_from_slice(&crate::ELF_MAGIC);
        img[4] = crate::ELFCLASS64;
        img[5] = crate::ELFDATA2LSB;
        img[6] = EV_CURRENT;
        img[0x10..0x12].copy_from_slice(&ET_EXEC.to_le_bytes());
        img[0x12..0x14].copy_from_slice(&EM_X86_64.to_le_bytes());
        img[0x14..0x18].copy_from_slice(&u32::from(EV_CURRENT).to_le_bytes());
        img[0x18..0x20].copy_from_slice(&0x401000u64.to_le_bytes());
        img[0x20..0x28].copy_from_slice(&(crate::EHDR_SIZE as u64).to_le_bytes());
        img[0x36..0x38].copy_from_slice(&PHDR_MIN_SIZE.to_le_bytes());
        img[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());

        let o = crate::EHDR_SIZE;
        img[o..o + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        img[o + 4..o + 8].copy_from_slice(&(PF_R | PF_X).to_le_bytes());
        img[o + 8..o + 16].copy_from_slice(&0x100u64.to_le_bytes());
        img[o + 16..o + 24].copy_from_slice(&0x401000u64.to_le_bytes());
        img[o + 24..o + 32].copy_from_slice(&0x401000u64.to_le_bytes());
        img[o + 32..o + 40].copy_from_slice(&0x100u64.to_le_bytes());
        img[o + 40..o + 48].copy_from_slice(&0x100u64.to_le_bytes());
        img[o + 48..o + 56].copy_from_slice(&PAGE_SIZE.to_le_bytes());

        let loaded = LoadedElf::parse(&img).unwrap();
        assert_eq!(loaded.phdr_vaddr, None);
        assert_eq!(loaded.phnum, 1);

        let auxv = AuxvInfo::from_loaded(&loaded);
        assert_eq!(auxv.phdr, 0);
        assert_eq!(auxv.phnum, 0);

        let mut buf = vec![0u8; 1024];
        let stack_bottom = USER_STACK_TOP - buf.len() as u64;
        let info = build_initial_stack(&mut buf, USER_STACK_TOP, &[b"/tiny"], &[], b"/tiny", [5; 16], &auxv).unwrap();
        let mut cur = info.rsp + 4 * 8;
        let mut saw_phdr_zero = false;
        let mut saw_phnum_zero = false;
        loop {
            let k = read_u64_at(&buf, stack_bottom, cur);
            let v = read_u64_at(&buf, stack_bottom, cur + 8);
            cur += 16;
            if k == AT_PHDR {
                assert_eq!(v, 0);
                saw_phdr_zero = true;
            }
            if k == AT_PHNUM {
                assert_eq!(v, 0);
                saw_phnum_zero = true;
            }
            if k == AT_NULL {
                break;
            }
        }
        assert!(saw_phdr_zero);
        assert!(saw_phnum_zero);
    }

    #[test]
    fn auxv_from_loaded_with_interp_emits_at_base_and_main_program_headers() {
        use crate::loader::{
            EM_X86_64, ET_DYN, EV_CURRENT, INTERP_BASE, PF_R, PF_X, PHDR_MIN_SIZE, PIE_BASE, PT_INTERP, PT_LOAD,
        };

        let build_phdr =
            |buf: &mut [u8], idx: usize, p_type: u32, flags: u32, off: u64, va: u64, fsz: u64, msz: u64| {
                let o = crate::EHDR_SIZE + idx * usize::from(PHDR_MIN_SIZE);
                buf[o..o + 4].copy_from_slice(&p_type.to_le_bytes());
                buf[o + 4..o + 8].copy_from_slice(&flags.to_le_bytes());
                buf[o + 8..o + 16].copy_from_slice(&off.to_le_bytes());
                buf[o + 16..o + 24].copy_from_slice(&va.to_le_bytes());
                buf[o + 24..o + 32].copy_from_slice(&va.to_le_bytes());
                buf[o + 32..o + 40].copy_from_slice(&fsz.to_le_bytes());
                buf[o + 40..o + 48].copy_from_slice(&msz.to_le_bytes());
                buf[o + 48..o + 56].copy_from_slice(&PAGE_SIZE.to_le_bytes());
            };

        // Main ET_DYN binary with PT_INTERP at offset 0x200.
        let interp_str = b"/lib/ld-musl-x86_64.so.1\0";
        let mut main_img = vec![0u8; 0x2000];
        main_img[0..4].copy_from_slice(&crate::ELF_MAGIC);
        main_img[4] = crate::ELFCLASS64;
        main_img[5] = crate::ELFDATA2LSB;
        main_img[6] = EV_CURRENT;
        main_img[0x10..0x12].copy_from_slice(&ET_DYN.to_le_bytes());
        main_img[0x12..0x14].copy_from_slice(&EM_X86_64.to_le_bytes());
        main_img[0x14..0x18].copy_from_slice(&u32::from(EV_CURRENT).to_le_bytes());
        main_img[0x18..0x20].copy_from_slice(&0x1040u64.to_le_bytes());
        main_img[0x20..0x28].copy_from_slice(&(crate::EHDR_SIZE as u64).to_le_bytes());
        main_img[0x36..0x38].copy_from_slice(&PHDR_MIN_SIZE.to_le_bytes());
        main_img[0x38..0x3a].copy_from_slice(&3u16.to_le_bytes());
        main_img[0x200..0x200 + interp_str.len()].copy_from_slice(interp_str);
        build_phdr(&mut main_img, 0, PT_INTERP, PF_R, 0x200, 0x200, interp_str.len() as u64, interp_str.len() as u64);
        build_phdr(&mut main_img, 1, PT_LOAD, PF_R, 0, 0, 0x400, 0x400);
        build_phdr(&mut main_img, 2, PT_LOAD, PF_R | PF_X, 0x1000, 0x1000, 0x100, 0x100);

        let main = LoadedElf::parse_with_pie_base(&main_img, PIE_BASE).unwrap();
        assert_eq!(main.interp_path(&main_img), Some(b"/lib/ld-musl-x86_64.so.1".as_slice()));

        // ET_DYN interpreter with min_vaddr = 0x2000 -> load_bias = INTERP_BASE - 0x2000.
        let mut interp_img = vec![0u8; 0x2000];
        interp_img[0..4].copy_from_slice(&crate::ELF_MAGIC);
        interp_img[4] = crate::ELFCLASS64;
        interp_img[5] = crate::ELFDATA2LSB;
        interp_img[6] = EV_CURRENT;
        interp_img[0x10..0x12].copy_from_slice(&ET_DYN.to_le_bytes());
        interp_img[0x12..0x14].copy_from_slice(&EM_X86_64.to_le_bytes());
        interp_img[0x14..0x18].copy_from_slice(&u32::from(EV_CURRENT).to_le_bytes());
        interp_img[0x18..0x20].copy_from_slice(&0x3080u64.to_le_bytes());
        interp_img[0x20..0x28].copy_from_slice(&(crate::EHDR_SIZE as u64).to_le_bytes());
        interp_img[0x36..0x38].copy_from_slice(&PHDR_MIN_SIZE.to_le_bytes());
        interp_img[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes());
        build_phdr(&mut interp_img, 0, PT_LOAD, PF_R, 0, 0x2000, 0x200, 0x200);
        build_phdr(&mut interp_img, 1, PT_LOAD, PF_R | PF_X, 0x1000, 0x3000, 0x100, 0x100);

        let interp = LoadedElf::parse_interp(&interp_img, INTERP_BASE).unwrap();
        assert_eq!(interp.load_bias, INTERP_BASE - 0x2000);

        let auxv = AuxvInfo::from_loaded_with_interp(&main, interp.load_bias);
        assert_eq!(auxv.base, Some(INTERP_BASE - 0x2000));
        assert_eq!(auxv.phdr, PIE_BASE + 64);
        assert_eq!(auxv.phnum, 3);
        assert_eq!(auxv.entry, PIE_BASE + 0x1040);

        let all_args: [&[u8]; 3] = [b"/bin/busybox", b"ls", b"-la"];
        let all_envs: [&[u8]; 2] = [b"PATH=/bin", b"HOME=/root"];
        for argc in 0..=3 {
            for envc in 0..=2 {
                let mut buf = vec![0u8; 4096];
                let stack_bottom = USER_STACK_TOP - buf.len() as u64;
                let info = build_initial_stack(
                    &mut buf,
                    USER_STACK_TOP,
                    &all_args[..argc],
                    &all_envs[..envc],
                    b"/bin/busybox",
                    [0x11; 16],
                    &auxv,
                )
                .unwrap();
                assert_eq!(info.rsp % 16, 0);

                // Skip argc, argv + NULL, envp + NULL to reach auxv.
                let mut cur = info.rsp + (1 + argc as u64 + 1 + envc as u64 + 1) * 8;
                let mut got_base = None;
                let mut got_phdr = None;
                let mut got_phnum = None;
                let mut got_entry = None;
                loop {
                    let k = read_u64_at(&buf, stack_bottom, cur);
                    let v = read_u64_at(&buf, stack_bottom, cur + 8);
                    cur += 16;
                    match k {
                        AT_BASE => got_base = Some(v),
                        AT_PHDR => got_phdr = Some(v),
                        AT_PHNUM => got_phnum = Some(v),
                        AT_ENTRY => got_entry = Some(v),
                        AT_NULL => break,
                        _ => {}
                    }
                }
                assert_eq!(got_base, Some(INTERP_BASE - 0x2000));
                assert_eq!(got_phdr, Some(PIE_BASE + 64));
                assert_eq!(got_phnum, Some(3));
                assert_eq!(got_entry, Some(PIE_BASE + 0x1040));
            }
        }
    }
}
