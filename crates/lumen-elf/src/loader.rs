//! ELF64 (`x86_64`) program-header parser, validator, and page planner.
//!
//! Validates static `ET_EXEC` / PIE `ET_DYN` executables, dynamic `PT_INTERP`
//! executables, and `ET_DYN` dynamic interpreters according to
//! `docs/ARCHITECTURE.md` §5 and §8:
//! - ELF64 little-endian, version 1, `EM_X86_64`, `ET_EXEC` or `ET_DYN`
//! - Static-only [`LoadedElf::parse`] rejects `PT_INTERP`; [`LoadedElf::parse_with_pie_base`]
//!   parses optional single `PT_INTERP` and requires mapped `AT_PHDR` when present;
//!   [`LoadedElf::parse_interp`] validates `ET_DYN` interpreters (`NestedInterp`,
//!   `load_bias = base - align_down(min_vaddr)`)
//! - Bounds-checks the program header table and every `PT_LOAD` file slice
//! - Validates `p_filesz <= p_memsz`, `USER_MIN..USER_MAX` bounds, W^X (both
//!   per-segment and across shared 4 KiB boundary pages), and non-overlapping
//!   byte ranges
//! - Handles RELRO adjacent `R--`/`RW-` segments sharing a 4 KiB boundary page
//!   by unioning page permissions without overwriting bytes from earlier segments
//! - Computes exact mapped `AT_PHDR`, `phent`, `phnum`, `entry`, and
//!   page-aligned `initial_brk`

use alloc::vec::Vec;
use core::fmt;

use crate::{EHDR_SIZE, ELF_MAGIC, ELFCLASS64, ELFDATA2LSB, read_u16, read_u32, read_u64};

/// Architectural 4 KiB page size.
pub const PAGE_SIZE: u64 = 4096;
/// Lowest valid user virtual address (64 KiB null guard below).
pub const USER_MIN: u64 = 0x0000_0000_0001_0000;
/// Exclusive upper bound of user virtual memory (top canonical page unmapped).
pub const USER_MAX: u64 = 0x0000_7fff_ffff_f000;
/// Default load bias for static PIE (`ET_DYN`) binaries.
pub const PIE_BASE: u64 = 0x0000_5555_5555_4000;
/// Default load base for the dynamic linker (`ld-musl`).
pub const INTERP_BASE: u64 = 0x0000_7f00_0000_0000;
/// Maximum byte length of a `PT_INTERP` interpreter path (including trailing NUL).
pub const PATH_MAX: usize = 4096;

/// ELF version (`EV_CURRENT`).
pub const EV_CURRENT: u8 = 1;
/// Static executable ELF type.
pub const ET_EXEC: u16 = 2;
/// Position-independent executable / shared object ELF type.
pub const ET_DYN: u16 = 3;
/// AMD x86-64 architecture (`EM_X86_64`).
pub const EM_X86_64: u16 = 62;
/// Minimum byte size of an ELF64 program header (`sizeof(Elf64_Phdr)`).
pub const PHDR_MIN_SIZE: u16 = 56;

/// Unused program header entry.
pub const PT_NULL: u32 = 0;
/// Loadable segment.
pub const PT_LOAD: u32 = 1;
/// Dynamic linking information.
pub const PT_DYNAMIC: u32 = 2;
/// Path to dynamic interpreter.
pub const PT_INTERP: u32 = 3;
/// Auxiliary information.
pub const PT_NOTE: u32 = 4;
/// Location and size of the program header table in memory.
pub const PT_PHDR: u32 = 6;
/// Thread-local storage template.
pub const PT_TLS: u32 = 7;
/// GNU stack permissions.
pub const PT_GNU_STACK: u32 = 0x6474_e551;
/// GNU read-only after relocation.
pub const PT_GNU_RELRO: u32 = 0x6474_e552;

/// Segment execute permission (`PF_X`).
pub const PF_X: u32 = 1;
/// Segment write permission (`PF_W`).
pub const PF_W: u32 = 2;
/// Segment read permission (`PF_R`).
pub const PF_R: u32 = 4;

/// Round `addr` down to a multiple of `PAGE_SIZE`.
#[inline]
pub const fn align_down(addr: u64) -> u64 {
    addr & !(PAGE_SIZE - 1)
}

/// Round `addr` up to a multiple of `PAGE_SIZE`, returning `None` on overflow.
#[inline]
pub const fn checked_align_up(addr: u64) -> Option<u64> {
    match addr.checked_add(PAGE_SIZE - 1) {
        Some(v) => Some(v & !(PAGE_SIZE - 1)),
        None => None,
    }
}

/// Memory access permissions for a segment or mapped page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Perm {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Perm {
    pub const NONE: Self = Self { read: false, write: false, exec: false };
    pub const R: Self = Self { read: true, write: false, exec: false };
    pub const RW: Self = Self { read: true, write: true, exec: false };
    pub const RX: Self = Self { read: true, write: false, exec: true };

    /// Decode ELF `p_flags` (`PF_R`, `PF_W`, `PF_X`).
    pub const fn from_p_flags(p_flags: u32) -> Self {
        Self { read: (p_flags & PF_R) != 0, write: (p_flags & PF_W) != 0, exec: (p_flags & PF_X) != 0 }
    }

    /// Encode back into ELF `p_flags` bits.
    pub const fn to_p_flags(self) -> u32 {
        (if self.read { PF_R } else { 0 }) | (if self.write { PF_W } else { 0 }) | (if self.exec { PF_X } else { 0 })
    }

    /// Union of two permission sets (used on shared 4 KiB boundary pages).
    pub const fn union(self, other: Self) -> Self {
        Self { read: self.read || other.read, write: self.write || other.write, exec: self.exec || other.exec }
    }

    /// True if both writable and executable (violates W^X).
    pub const fn is_wx(self) -> bool {
        self.write && self.exec
    }
}

impl fmt::Display for Perm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}{}",
            if self.read { 'R' } else { '-' },
            if self.write { 'W' } else { '-' },
            if self.exec { 'X' } else { '-' },
        )
    }
}

/// Supported static ELF64 image kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfType {
    /// Fixed-address executable (`ET_EXEC`, `load_bias = 0`).
    Exec,
    /// Static position-independent executable (`ET_DYN` without `PT_INTERP`).
    Pie,
}

/// Raw parsed ELF64 program header (`Elf64_Phdr`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

/// A validated `PT_LOAD` segment with `p_memsz > 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadSegment {
    /// Exact mapped user virtual start address (`load_bias + p_vaddr`).
    pub vaddr: u64,
    /// In-memory byte length (`p_memsz > 0`).
    pub memsz: u64,
    /// Start offset in the ELF image (`p_offset`).
    pub file_offset: usize,
    /// Initialized byte length in the ELF image (`p_filesz <= p_memsz`).
    pub filesz: usize,
    /// Requested alignment from `p_align`.
    pub align: u64,
    /// Segment permissions (guaranteed not W+X).
    pub perm: Perm,
}

impl LoadSegment {
    /// Exclusive byte end address (`vaddr + memsz`).
    #[inline]
    pub const fn end(&self) -> u64 {
        self.vaddr + self.memsz
    }

    /// Page-aligned start address covering this segment.
    #[inline]
    pub const fn page_start(&self) -> u64 {
        align_down(self.vaddr)
    }

    /// Exclusive page-aligned end address covering this segment.
    #[inline]
    pub const fn page_end(&self) -> u64 {
        // Validated at parse time that `end <= USER_MAX` (which is page-aligned).
        (self.end() + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
    }

    /// Number of trailing zero-filled `.bss` bytes (`memsz - filesz`).
    #[inline]
    pub const fn bss_len(&self) -> u64 {
        self.memsz - self.filesz as u64
    }

    /// Slice of initialized file bytes inside `image` for this segment.
    ///
    /// Returns `None` only if called with a different/shorter slice than the
    /// validated ELF image.
    #[inline]
    pub fn file_bytes<'a>(&self, image: &'a [u8]) -> Option<&'a [u8]> {
        image.get(self.file_offset..self.file_offset.checked_add(self.filesz)?)
    }
}

/// One step in loading a validated ELF into user virtual memory.
///
/// Executing the sequence returned by [`LoadedElf::plan_pages`] in order maps
/// every page exactly once (zero-initialized), upgrades permissions on shared
/// RELRO 4 KiB boundary pages without clobbering earlier bytes, and copies each
/// segment's initialized file bytes to its exact user VA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageAction {
    /// Map `page_count` fresh zero-initialized 4 KiB pages starting at `vaddr`
    /// (page-aligned) with `perm`.
    MapZero { vaddr: u64, page_count: u64, perm: Perm },
    /// Upgrade permissions on the already-mapped 4 KiB page at `vaddr` to
    /// `perm` (union of all segments sharing the page), preserving its bytes.
    UpdatePerm { vaddr: u64, perm: Perm },
    /// Copy `len` initialized bytes from `image[file_offset..file_offset + len]`
    /// to exact user VA `vaddr`.
    CopyBytes { vaddr: u64, file_offset: usize, len: usize },
}

/// A coalesced, non-overlapping, page-aligned virtual mapping with its final
/// permissions (including unioned permissions on shared 4 KiB boundary pages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedRange {
    pub start: u64,
    pub end: u64,
    pub perm: Perm,
}

impl MappedRange {
    /// Number of 4 KiB pages covered by this range.
    #[inline]
    pub const fn page_count(&self) -> u64 {
        (self.end - self.start) / PAGE_SIZE
    }
}

/// Why an ELF64 image could not be loaded as a static x86-64 user binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfLoadError {
    /// Image is shorter than the ELF header or its program-header table.
    Truncated,
    /// Missing `\x7fELF` magic bytes.
    BadMagic,
    /// Not `ELFCLASS64`.
    BadClass(u8),
    /// Not `ELFDATA2LSB` (little-endian).
    BadEndian(u8),
    /// `e_ident[EI_VERSION]` or `e_version` is not `1`.
    BadVersion,
    /// `e_machine` is not `EM_X86_64` (`62`).
    UnsupportedMachine(u16),
    /// `e_type` is neither `ET_EXEC` (`2`) nor `ET_DYN` (`3`).
    UnsupportedType(u16),
    /// Image has a `PT_INTERP` segment; static-only loader refused it.
    DynamicInterpreterUnsupported,
    /// Image contains more than one `PT_INTERP` program header.
    DuplicateInterp,
    /// `PT_INTERP` file slice overflows or extends past the end of the image.
    InterpOutOfBounds,
    /// `PT_INTERP` path is empty, exceeds [`PATH_MAX`], lacks trailing NUL, or has interior NUL.
    InvalidInterp,
    /// Dynamic ELF (`PT_INTERP` present) lacks a readable mapped `AT_PHDR`.
    DynamicMissingPhdr,
    /// Interpreter ELF itself contains a `PT_INTERP` segment.
    NestedInterp,
    /// Interpreter ELF is not `ET_DYN`.
    InterpreterNotDynamic(u16),
    /// Program header table geometry (`e_phentsize < 56` or overflow) is invalid.
    BadProgramHeaderTable,
    /// Caller-provided `pie_base` is misaligned or outside `USER_MIN..USER_MAX`.
    InvalidPieBase(u64),
    /// A `PT_LOAD` segment has `p_filesz > p_memsz`.
    FileSizeExceedsMemSize { index: usize },
    /// A `PT_LOAD` segment's file bytes extend past the end of the image (or overflow).
    SegmentFileOutOfBounds { index: usize },
    /// Arithmetic overflow computing a segment's mapped address or end.
    AddressOverflow { index: usize },
    /// A `PT_LOAD` segment maps outside `USER_MIN..USER_MAX`.
    AddressOutOfRange { index: usize, start: u64, end: u64 },
    /// A single `PT_LOAD` segment requests both `PF_W` and `PF_X`.
    WritableAndExecutable { index: usize },
    /// Adjacent segments sharing a 4 KiB boundary page union to both `W` and `X`.
    SharedPageWritableAndExecutable { page: u64 },
    /// Two `PT_LOAD` segments overlap in virtual byte range `[start, end)`.
    OverlappingSegments { first: usize, second: usize },
    /// No `PT_LOAD` segment with `p_memsz > 0` was found.
    NoLoadSegments,
    /// Arithmetic overflow computing `load_bias + e_entry`.
    EntryOverflow,
    /// Entry point does not lie inside any executable `PT_LOAD` segment.
    EntryNotExecutable { entry: u64 },
}

impl fmt::Display for ElfLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Truncated => f.write_str("truncated ELF64 image"),
            Self::BadMagic => f.write_str("missing ELF magic"),
            Self::BadClass(c) => write!(f, "unsupported ELF class {c} (expected ELFCLASS64)"),
            Self::BadEndian(d) => write!(f, "unsupported ELF endianness {d} (expected little-endian)"),
            Self::BadVersion => f.write_str("unsupported ELF version (expected 1)"),
            Self::UnsupportedMachine(m) => write!(f, "unsupported ELF machine {m} (expected EM_X86_64)"),
            Self::UnsupportedType(t) => write!(f, "unsupported ELF type {t} (expected ET_EXEC or ET_DYN)"),
            Self::DynamicInterpreterUnsupported => f.write_str("dynamic interpreter (PT_INTERP) not supported"),
            Self::DuplicateInterp => f.write_str("duplicate PT_INTERP program header"),
            Self::InterpOutOfBounds => f.write_str("PT_INTERP slice out of bounds"),
            Self::InvalidInterp => f.write_str("invalid PT_INTERP interpreter path"),
            Self::DynamicMissingPhdr => f.write_str("dynamic ELF missing readable mapped AT_PHDR"),
            Self::NestedInterp => f.write_str("interpreter ELF must not contain PT_INTERP"),
            Self::InterpreterNotDynamic(t) => write!(f, "interpreter ELF type {t} is not ET_DYN"),
            Self::BadProgramHeaderTable => f.write_str("malformed ELF program header table"),
            Self::InvalidPieBase(b) => write!(f, "invalid PIE load base {b:#x}"),
            Self::FileSizeExceedsMemSize { index } => write!(f, "PT_LOAD[{index}]: p_filesz > p_memsz"),
            Self::SegmentFileOutOfBounds { index } => write!(f, "PT_LOAD[{index}]: file range out of bounds"),
            Self::AddressOverflow { index } => write!(f, "PT_LOAD[{index}]: virtual address overflow"),
            Self::AddressOutOfRange { index, start, end } => {
                write!(f, "PT_LOAD[{index}]: range {start:#x}..{end:#x} outside user space")
            }
            Self::WritableAndExecutable { index } => write!(f, "PT_LOAD[{index}]: W+X segment rejected"),
            Self::SharedPageWritableAndExecutable { page } => {
                write!(f, "shared boundary page {page:#x} unions to W+X")
            }
            Self::OverlappingSegments { first, second } => {
                write!(f, "PT_LOAD[{first}] and PT_LOAD[{second}] overlap in virtual memory")
            }
            Self::NoLoadSegments => f.write_str("ELF image contains no loadable PT_LOAD segments"),
            Self::EntryOverflow => f.write_str("entry point virtual address overflow"),
            Self::EntryNotExecutable { entry } => {
                write!(f, "entry point {entry:#x} is not inside an executable PT_LOAD segment")
            }
        }
    }
}

/// Validated ELF64 program-header load plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedElf {
    /// `ET_EXEC` or `ET_DYN` (PIE / interpreter).
    pub elf_type: ElfType,
    /// Load bias applied to all virtual addresses (`0` for `ET_EXEC`, `PIE_BASE` for `ET_DYN`).
    pub load_bias: u64,
    /// Mapped entry-point user VA (`load_bias + e_entry`).
    pub entry: u64,
    /// Exact mapped user VA of the program header table for `AT_PHDR`, if
    /// covered by a loaded segment.
    pub phdr_vaddr: Option<u64>,
    /// Size of each program header entry (`e_phentsize`) for `AT_PHENT`.
    pub phent: u16,
    /// Number of program headers (`e_phnum`) for `AT_PHNUM`.
    pub phnum: u16,
    /// Initial page-aligned `brk` immediately above the highest `PT_LOAD` page end.
    pub initial_brk: u64,
    segments: Vec<LoadSegment>,
    /// Optional validated `(file_offset, len_without_nul)` of `PT_INTERP`.
    interp: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseMode {
    /// Static-only (`LoadedElf::parse`): rejects `PT_INTERP` immediately.
    StaticOnly,
    /// Main executable (`LoadedElf::parse_with_pie_base`): accepts optional single `PT_INTERP`.
    AllowInterp,
    /// Dynamic interpreter (`LoadedElf::parse_interp`): requires `ET_DYN`, rejects `PT_INTERP`
    /// with `NestedInterp`, and sets `load_bias = base.checked_sub(align_down(min_vaddr))`.
    Interpreter,
}

impl LoadedElf {
    /// Validate and plan a static ELF64 image, using [`PIE_BASE`] for `ET_DYN`.
    ///
    /// Rejects `PT_INTERP` with [`ElfLoadError::DynamicInterpreterUnsupported`].
    pub fn parse(image: &[u8]) -> Result<Self, ElfLoadError> {
        Self::parse_inner(image, PIE_BASE, ParseMode::StaticOnly)
    }

    /// Validate and plan a static or dynamically linked (`PT_INTERP`) ELF64
    /// image, using `pie_base` as the load bias when the binary is `ET_DYN`.
    pub fn parse_with_pie_base(image: &[u8], pie_base: u64) -> Result<Self, ElfLoadError> {
        Self::parse_inner(image, pie_base, ParseMode::AllowInterp)
    }

    /// Validate and plan a dynamic interpreter (`ET_DYN` without `PT_INTERP`),
    /// computing `load_bias = base.checked_sub(align_down(min_vaddr))`.
    pub fn parse_interp(image: &[u8], base: u64) -> Result<Self, ElfLoadError> {
        Self::parse_inner(image, base, ParseMode::Interpreter)
    }

    fn parse_inner(image: &[u8], base: u64, mode: ParseMode) -> Result<Self, ElfLoadError> {
        if image.len() < EHDR_SIZE {
            return Err(ElfLoadError::Truncated);
        }
        if image[0..4] != ELF_MAGIC {
            return Err(ElfLoadError::BadMagic);
        }
        if image[4] != ELFCLASS64 {
            return Err(ElfLoadError::BadClass(image[4]));
        }
        if image[5] != ELFDATA2LSB {
            return Err(ElfLoadError::BadEndian(image[5]));
        }
        if image[6] != EV_CURRENT {
            return Err(ElfLoadError::BadVersion);
        }

        let e_type = read_u16(image, 0x10).ok_or(ElfLoadError::Truncated)?;
        let e_machine = read_u16(image, 0x12).ok_or(ElfLoadError::Truncated)?;
        let e_version = read_u32(image, 0x14).ok_or(ElfLoadError::Truncated)?;
        let e_entry = read_u64(image, 0x18).ok_or(ElfLoadError::Truncated)?;
        let e_phoff = read_u64(image, 0x20).ok_or(ElfLoadError::Truncated)?;
        let e_phentsize = read_u16(image, 0x36).ok_or(ElfLoadError::Truncated)?;
        let e_phnum = read_u16(image, 0x38).ok_or(ElfLoadError::Truncated)?;

        if e_version != u32::from(EV_CURRENT) {
            return Err(ElfLoadError::BadVersion);
        }
        if e_machine != EM_X86_64 {
            return Err(ElfLoadError::UnsupportedMachine(e_machine));
        }

        let (elf_type, raw_load_bias) = match (mode, e_type) {
            (ParseMode::Interpreter, ET_DYN) | (ParseMode::StaticOnly | ParseMode::AllowInterp, ET_DYN) => {
                if !(USER_MIN..USER_MAX).contains(&base) || !base.is_multiple_of(PAGE_SIZE) {
                    return Err(ElfLoadError::InvalidPieBase(base));
                }
                (ElfType::Pie, base)
            }
            (ParseMode::Interpreter, ET_EXEC) => return Err(ElfLoadError::InterpreterNotDynamic(ET_EXEC)),
            (ParseMode::StaticOnly | ParseMode::AllowInterp, ET_EXEC) => (ElfType::Exec, 0u64),
            (_, other) => return Err(ElfLoadError::UnsupportedType(other)),
        };

        if e_phnum == 0 {
            return Err(ElfLoadError::NoLoadSegments);
        }
        if e_phentsize < PHDR_MIN_SIZE {
            return Err(ElfLoadError::BadProgramHeaderTable);
        }

        let ph_bytes =
            u64::from(e_phnum).checked_mul(u64::from(e_phentsize)).ok_or(ElfLoadError::BadProgramHeaderTable)?;
        let ph_end = e_phoff.checked_add(ph_bytes).ok_or(ElfLoadError::BadProgramHeaderTable)?;
        if ph_end > image.len() as u64 {
            return Err(ElfLoadError::Truncated);
        }
        let phoff_usize = usize::try_from(e_phoff).map_err(|_| ElfLoadError::Truncated)?;
        let phent_usize = usize::from(e_phentsize);
        let ph_bytes_usize = usize::try_from(ph_bytes).map_err(|_| ElfLoadError::Truncated)?;

        let load_bias = if mode == ParseMode::Interpreter {
            let mut min_vaddr: Option<u64> = None;
            for index in 0..usize::from(e_phnum) {
                let off = phoff_usize + index * phent_usize;
                let ph = parse_phdr(image, off).ok_or(ElfLoadError::Truncated)?;
                if ph.p_type == PT_INTERP {
                    return Err(ElfLoadError::NestedInterp);
                }
                if ph.p_type == PT_LOAD && ph.p_memsz > 0 {
                    min_vaddr = Some(min_vaddr.map_or(ph.p_vaddr, |m| m.min(ph.p_vaddr)));
                }
            }
            let min_page = align_down(min_vaddr.unwrap_or(0));
            base.checked_sub(min_page).ok_or(ElfLoadError::InvalidPieBase(base))?
        } else {
            raw_load_bias
        };

        let mut pt_phdr: Option<ProgramHeader> = None;
        let mut interp: Option<(usize, usize)> = None;
        let mut indexed: Vec<(usize, LoadSegment)> = Vec::new();

        for index in 0..usize::from(e_phnum) {
            let off = phoff_usize + index * phent_usize;
            let ph = parse_phdr(image, off).ok_or(ElfLoadError::Truncated)?;
            match ph.p_type {
                PT_INTERP => match mode {
                    ParseMode::StaticOnly => return Err(ElfLoadError::DynamicInterpreterUnsupported),
                    ParseMode::Interpreter => return Err(ElfLoadError::NestedInterp),
                    ParseMode::AllowInterp => {
                        if interp.is_some() {
                            return Err(ElfLoadError::DuplicateInterp);
                        }
                        if ph.p_filesz <= 1 || ph.p_filesz > PATH_MAX as u64 {
                            return Err(ElfLoadError::InvalidInterp);
                        }
                        let interp_end = ph.p_offset.checked_add(ph.p_filesz).ok_or(ElfLoadError::InterpOutOfBounds)?;
                        if interp_end > image.len() as u64 {
                            return Err(ElfLoadError::InterpOutOfBounds);
                        }
                        let interp_off = usize::try_from(ph.p_offset).map_err(|_| ElfLoadError::InterpOutOfBounds)?;
                        let interp_len = usize::try_from(ph.p_filesz).map_err(|_| ElfLoadError::InterpOutOfBounds)?;
                        let bytes =
                            image.get(interp_off..interp_off + interp_len).ok_or(ElfLoadError::InterpOutOfBounds)?;
                        if *bytes.last().unwrap_or(&1) != 0 || bytes[..interp_len - 1].contains(&0) {
                            return Err(ElfLoadError::InvalidInterp);
                        }
                        interp = Some((interp_off, interp_len - 1));
                    }
                },
                PT_PHDR => {
                    if pt_phdr.is_none() {
                        pt_phdr = Some(ph);
                    }
                }
                PT_LOAD => {
                    if ph.p_filesz > ph.p_memsz {
                        return Err(ElfLoadError::FileSizeExceedsMemSize { index });
                    }
                    if ph.p_memsz == 0 {
                        continue;
                    }
                    let file_end =
                        ph.p_offset.checked_add(ph.p_filesz).ok_or(ElfLoadError::SegmentFileOutOfBounds { index })?;
                    if file_end > image.len() as u64 {
                        return Err(ElfLoadError::SegmentFileOutOfBounds { index });
                    }
                    let perm = Perm::from_p_flags(ph.p_flags);
                    if perm.is_wx() {
                        return Err(ElfLoadError::WritableAndExecutable { index });
                    }
                    let vaddr = load_bias.checked_add(ph.p_vaddr).ok_or(ElfLoadError::AddressOverflow { index })?;
                    let end = vaddr.checked_add(ph.p_memsz).ok_or(ElfLoadError::AddressOverflow { index })?;
                    let page_start = align_down(vaddr);
                    let page_end = checked_align_up(end).ok_or(ElfLoadError::AddressOverflow { index })?;
                    if vaddr < USER_MIN || page_start < USER_MIN || end > USER_MAX || page_end > USER_MAX {
                        return Err(ElfLoadError::AddressOutOfRange { index, start: vaddr, end });
                    }
                    let file_offset =
                        usize::try_from(ph.p_offset).map_err(|_| ElfLoadError::SegmentFileOutOfBounds { index })?;
                    let filesz =
                        usize::try_from(ph.p_filesz).map_err(|_| ElfLoadError::SegmentFileOutOfBounds { index })?;
                    indexed.push((
                        index,
                        LoadSegment { vaddr, memsz: ph.p_memsz, file_offset, filesz, align: ph.p_align, perm },
                    ));
                }
                _ => {}
            }
        }

        if indexed.is_empty() {
            return Err(ElfLoadError::NoLoadSegments);
        }

        indexed.sort_by_key(|(_, seg)| seg.vaddr);

        // Enforce non-overlapping byte ranges across all PT_LOAD segments.
        for pair in indexed.windows(2) {
            let (idx_a, seg_a) = pair[0];
            let (idx_b, seg_b) = pair[1];
            if seg_a.end() > seg_b.vaddr {
                return Err(ElfLoadError::OverlappingSegments { first: idx_a, second: idx_b });
            }
        }

        let segments: Vec<LoadSegment> = indexed.into_iter().map(|(_, s)| s).collect();

        // Enforce W^X across shared 4 KiB boundary pages (including 3+ segments
        // sharing the same page).
        let mut last_page: Option<(u64, Perm)> = None;
        for seg in &segments {
            let p_start = seg.page_start();
            let p_end = seg.page_end();
            if let Some((prev_page, prev_perm)) = last_page
                && prev_page == p_start
            {
                let unioned = prev_perm.union(seg.perm);
                if unioned.is_wx() {
                    return Err(ElfLoadError::SharedPageWritableAndExecutable { page: p_start });
                }
                if p_end == p_start + PAGE_SIZE {
                    last_page = Some((p_start, unioned));
                } else {
                    last_page = Some((p_end - PAGE_SIZE, seg.perm));
                }
            } else {
                last_page = Some((p_end - PAGE_SIZE, seg.perm));
            }
        }

        // Validate entry point lies inside an executable PT_LOAD segment.
        let entry = load_bias.checked_add(e_entry).ok_or(ElfLoadError::EntryOverflow)?;
        if !segments.iter().any(|s| s.perm.exec && entry >= s.vaddr && entry < s.end()) {
            return Err(ElfLoadError::EntryNotExecutable { entry });
        }

        // Compute AT_PHDR: prefer PT_PHDR if it lies inside a loaded segment;
        // otherwise use the PT_LOAD whose file bytes cover `e_phoff..e_phoff+ph_bytes`.
        let phdr_vaddr = compute_at_phdr(&segments, load_bias, pt_phdr, phoff_usize, ph_bytes_usize);
        if interp.is_some() && phdr_vaddr.is_none() {
            return Err(ElfLoadError::DynamicMissingPhdr);
        }

        // Highest page end across all PT_LOAD segments (already sorted and non-overlapping).
        let initial_brk = segments.last().map_or(USER_MIN, LoadSegment::page_end);

        Ok(Self {
            elf_type,
            load_bias,
            entry,
            phdr_vaddr,
            phent: e_phentsize,
            phnum: e_phnum,
            initial_brk,
            segments,
            interp,
        })
    }

    /// Validated `PT_LOAD` segments sorted by ascending virtual address.
    pub fn segments(&self) -> &[LoadSegment] {
        &self.segments
    }

    /// `true` if the ELF contains a validated `PT_INTERP` header.
    pub const fn has_interp(&self) -> bool {
        self.interp.is_some()
    }

    /// Slice of `image` holding the validated `PT_INTERP` path (without trailing NUL),
    /// or `None` for static binaries.
    pub fn interp_path<'a>(&self, image: &'a [u8]) -> Option<&'a [u8]> {
        let (off, len) = self.interp?;
        image.get(off..off.checked_add(len)?)
    }

    /// Mapped user VA of the program header table for `AT_PHDR`, or `0` if unmapped.
    pub const fn at_phdr(&self) -> u64 {
        match self.phdr_vaddr {
            Some(va) => va,
            None => 0,
        }
    }

    /// Produce an ordered sequence of page mapping, permission-union, and byte
    /// copy actions that loads this ELF cleanly without clobbering bytes on
    /// shared 4 KiB RELRO boundary pages.
    pub fn plan_pages(&self) -> Vec<PageAction> {
        let mut actions = Vec::new();
        let mut last_page: Option<(u64, Perm)> = None;

        for seg in &self.segments {
            let p_start = seg.page_start();
            let p_end = seg.page_end();

            if let Some((prev_page, prev_perm)) = last_page
                && prev_page == p_start
            {
                let unioned = prev_perm.union(seg.perm);
                if unioned != prev_perm {
                    actions.push(PageAction::UpdatePerm { vaddr: p_start, perm: unioned });
                }
                let new_start = p_start + PAGE_SIZE;
                if p_end > new_start {
                    actions.push(PageAction::MapZero {
                        vaddr: new_start,
                        page_count: (p_end - new_start) / PAGE_SIZE,
                        perm: seg.perm,
                    });
                    last_page = Some((p_end - PAGE_SIZE, seg.perm));
                } else {
                    last_page = Some((p_start, unioned));
                }
            } else {
                actions.push(PageAction::MapZero {
                    vaddr: p_start,
                    page_count: (p_end - p_start) / PAGE_SIZE,
                    perm: seg.perm,
                });
                last_page = Some((p_end - PAGE_SIZE, seg.perm));
            }

            if seg.filesz > 0 {
                actions.push(PageAction::CopyBytes { vaddr: seg.vaddr, file_offset: seg.file_offset, len: seg.filesz });
            }
        }

        actions
    }

    /// Produce coalesced, non-overlapping, page-aligned virtual memory ranges
    /// with their final unioned permissions (suitable for registering VMAs in
    /// `lumen-vm` before copying each segment's [`LoadSegment::file_bytes`]).
    pub fn mapped_ranges(&self) -> Vec<MappedRange> {
        let mut ranges: Vec<MappedRange> = Vec::new();

        for seg in &self.segments {
            let p_start = seg.page_start();
            let p_end = seg.page_end();

            if let Some(last) = ranges.last_mut()
                && last.end - PAGE_SIZE == p_start
            {
                let prev_perm = last.perm;
                let unioned = prev_perm.union(seg.perm);
                if unioned != prev_perm {
                    last.end -= PAGE_SIZE;
                    if last.start == last.end {
                        ranges.pop();
                    }
                    push_range(&mut ranges, p_start, p_start + PAGE_SIZE, unioned);
                }
                push_range(&mut ranges, p_start + PAGE_SIZE, p_end, seg.perm);
            } else {
                push_range(&mut ranges, p_start, p_end, seg.perm);
            }
        }

        ranges
    }
}

fn push_range(ranges: &mut Vec<MappedRange>, start: u64, end: u64, perm: Perm) {
    if start == end {
        return;
    }
    if let Some(last) = ranges.last_mut()
        && last.end == start
        && last.perm == perm
    {
        last.end = end;
    } else {
        ranges.push(MappedRange { start, end, perm });
    }
}

fn parse_phdr(image: &[u8], off: usize) -> Option<ProgramHeader> {
    Some(ProgramHeader {
        p_type: read_u32(image, off)?,
        p_flags: read_u32(image, off + 0x04)?,
        p_offset: read_u64(image, off + 0x08)?,
        p_vaddr: read_u64(image, off + 0x10)?,
        p_paddr: read_u64(image, off + 0x18)?,
        p_filesz: read_u64(image, off + 0x20)?,
        p_memsz: read_u64(image, off + 0x28)?,
        p_align: read_u64(image, off + 0x30)?,
    })
}

fn compute_at_phdr(
    segments: &[LoadSegment],
    load_bias: u64,
    pt_phdr: Option<ProgramHeader>,
    phoff: usize,
    ph_bytes: usize,
) -> Option<u64> {
    if let Some(ph) = pt_phdr
        && let Some(va) = load_bias.checked_add(ph.p_vaddr)
        && let Some(va_end) = va.checked_add(ph_bytes as u64)
        && segments.iter().any(|s| {
            s.perm.read
                && va >= s.vaddr
                && s.vaddr.checked_add(s.filesz as u64).is_some_and(|file_va_end| va_end <= file_va_end)
                && usize::try_from(va - s.vaddr)
                    .ok()
                    .and_then(|delta| s.file_offset.checked_add(delta))
                    .is_some_and(|mapped_off| mapped_off == phoff)
        })
    {
        return Some(va);
    }

    let ph_end = phoff.checked_add(ph_bytes)?;
    for seg in segments {
        if !seg.perm.read {
            continue;
        }
        let seg_file_end = seg.file_offset.checked_add(seg.filesz)?;
        if seg.file_offset <= phoff && ph_end <= seg_file_end {
            let delta = (phoff - seg.file_offset) as u64;
            return seg.vaddr.checked_add(delta);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::collections::BTreeMap;
    use std::{vec, vec::Vec};

    /// Helper to construct synthetic ELF64 images for testing.
    struct ElfBuilder {
        e_type: u16,
        e_machine: u16,
        e_version: u32,
        ei_class: u8,
        ei_data: u8,
        ei_version: u8,
        e_entry: u64,
        e_phentsize: u16,
        phdrs: Vec<ProgramHeader>,
        payload: Vec<u8>,
    }

    impl ElfBuilder {
        fn new(e_type: u16, e_entry: u64) -> Self {
            Self {
                e_type,
                e_machine: EM_X86_64,
                e_version: u32::from(EV_CURRENT),
                ei_class: ELFCLASS64,
                ei_data: ELFDATA2LSB,
                ei_version: EV_CURRENT,
                e_entry,
                e_phentsize: PHDR_MIN_SIZE,
                phdrs: Vec::new(),
                payload: Vec::new(),
            }
        }

        fn add_phdr(&mut self, ph: ProgramHeader) -> &mut Self {
            self.phdrs.push(ph);
            self
        }

        /// Append `data` at the current payload offset (after header + phdrs)
        /// and return its offset relative to payload start.
        fn push_data(&mut self, data: &[u8]) -> usize {
            let off = self.payload.len();
            self.payload.extend_from_slice(data);
            off
        }

        /// Build an ELF where phdrs sit at offset 64 (`EHDR_SIZE`), followed by
        /// payload starting at `payload_base = EHDR_SIZE + phdrs.len() * e_phentsize`.
        fn build_raw(&self) -> Vec<u8> {
            let phoff = EHDR_SIZE as u64;
            let phent = usize::from(self.e_phentsize);
            let mut out = vec![0u8; EHDR_SIZE];
            out[0..4].copy_from_slice(&ELF_MAGIC);
            out[4] = self.ei_class;
            out[5] = self.ei_data;
            out[6] = self.ei_version;
            out[0x10..0x12].copy_from_slice(&self.e_type.to_le_bytes());
            out[0x12..0x14].copy_from_slice(&self.e_machine.to_le_bytes());
            out[0x14..0x18].copy_from_slice(&self.e_version.to_le_bytes());
            out[0x18..0x20].copy_from_slice(&self.e_entry.to_le_bytes());
            out[0x20..0x28].copy_from_slice(&phoff.to_le_bytes());
            out[0x34..0x36].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
            out[0x36..0x38].copy_from_slice(&self.e_phentsize.to_le_bytes());
            out[0x38..0x3a].copy_from_slice(&(self.phdrs.len() as u16).to_le_bytes());

            for ph in &self.phdrs {
                let mut entry = vec![0u8; phent.max(usize::from(PHDR_MIN_SIZE))];
                entry[0x00..0x04].copy_from_slice(&ph.p_type.to_le_bytes());
                entry[0x04..0x08].copy_from_slice(&ph.p_flags.to_le_bytes());
                entry[0x08..0x10].copy_from_slice(&ph.p_offset.to_le_bytes());
                entry[0x10..0x18].copy_from_slice(&ph.p_vaddr.to_le_bytes());
                entry[0x18..0x20].copy_from_slice(&ph.p_paddr.to_le_bytes());
                entry[0x20..0x28].copy_from_slice(&ph.p_filesz.to_le_bytes());
                entry[0x28..0x30].copy_from_slice(&ph.p_memsz.to_le_bytes());
                entry[0x30..0x38].copy_from_slice(&ph.p_align.to_le_bytes());
                out.extend_from_slice(&entry[..phent]);
            }
            out.extend_from_slice(&self.payload);
            out
        }
    }

    fn load_phdr(flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64) -> ProgramHeader {
        ProgramHeader {
            p_type: PT_LOAD,
            p_flags: flags,
            p_offset: offset,
            p_vaddr: vaddr,
            p_paddr: vaddr,
            p_filesz: filesz,
            p_memsz: memsz,
            p_align: PAGE_SIZE,
        }
    }

    /// Execute `plan_pages` on a simulated page-mapped address space and return
    /// every mapped page's bytes and final permissions.
    fn execute_plan(loaded: &LoadedElf, image: &[u8]) -> BTreeMap<u64, ([u8; PAGE_SIZE as usize], Perm)> {
        let mut pages: BTreeMap<u64, ([u8; PAGE_SIZE as usize], Perm)> = BTreeMap::new();
        for action in loaded.plan_pages() {
            match action {
                PageAction::MapZero { vaddr, page_count, perm } => {
                    assert_eq!(vaddr % PAGE_SIZE, 0);
                    assert!(page_count > 0);
                    assert!(!perm.is_wx());
                    for i in 0..page_count {
                        let p = vaddr + i * PAGE_SIZE;
                        let prev = pages.insert(p, ([0u8; PAGE_SIZE as usize], perm));
                        assert!(prev.is_none(), "page {p:#x} mapped twice");
                    }
                }
                PageAction::UpdatePerm { vaddr, perm } => {
                    assert_eq!(vaddr % PAGE_SIZE, 0);
                    assert!(!perm.is_wx());
                    let entry = pages.get_mut(&vaddr).expect("UpdatePerm on unmapped page");
                    entry.1 = perm;
                }
                PageAction::CopyBytes { vaddr, file_offset, len } => {
                    let src = &image[file_offset..file_offset + len];
                    for (i, &byte) in src.iter().enumerate() {
                        let va = vaddr + i as u64;
                        let page = align_down(va);
                        let off = (va - page) as usize;
                        let entry = pages.get_mut(&page).expect("CopyBytes into unmapped page");
                        entry.0[off] = byte;
                    }
                }
            }
        }
        pages
    }

    /// Execute `mapped_ranges` + `segments` directly and assert it yields the
    /// exact same pages, permissions, and bytes as `plan_pages`.
    fn assert_plan_matches_ranges(loaded: &LoadedElf, image: &[u8]) -> BTreeMap<u64, ([u8; PAGE_SIZE as usize], Perm)> {
        let from_plan = execute_plan(loaded, image);
        let mut from_ranges: BTreeMap<u64, ([u8; PAGE_SIZE as usize], Perm)> = BTreeMap::new();
        for r in loaded.mapped_ranges() {
            assert_eq!(r.start % PAGE_SIZE, 0);
            assert_eq!(r.end % PAGE_SIZE, 0);
            assert!(r.start < r.end);
            assert!(!r.perm.is_wx());
            let mut p = r.start;
            while p < r.end {
                assert!(from_ranges.insert(p, ([0u8; PAGE_SIZE as usize], r.perm)).is_none());
                p += PAGE_SIZE;
            }
        }
        for seg in loaded.segments() {
            let src = seg.file_bytes(image).unwrap();
            for (i, &byte) in src.iter().enumerate() {
                let va = seg.vaddr + i as u64;
                let page = align_down(va);
                let off = (va - page) as usize;
                from_ranges.get_mut(&page).unwrap().0[off] = byte;
            }
        }
        assert_eq!(from_plan, from_ranges);
        from_plan
    }

    #[test]
    fn valid_et_exec_with_pt_phdr_and_bss() {
        // Headers at 0..0x1000 (mapped R-- at 0x400000), .text R-X at 0x401000,
        // .data + .bss RW- at 0x402000 (0x200 initialized, 0x1800 memsz).
        let mut b = ElfBuilder::new(ET_EXEC, 0x401010);
        let ph_size = 4 * u64::from(PHDR_MIN_SIZE);
        let hdr_pad = 0x1000 - (EHDR_SIZE + ph_size as usize);
        b.push_data(&vec![0x11; hdr_pad]); // pad to file offset 0x1000
        let text_bytes = vec![0x90u8; 0x100]; // NOPs
        b.push_data(&text_bytes);
        let data_bytes = vec![0xABu8; 0x200];
        b.push_data(&data_bytes);

        b.add_phdr(ProgramHeader {
            p_type: PT_PHDR,
            p_flags: PF_R,
            p_offset: EHDR_SIZE as u64,
            p_vaddr: 0x400000 + EHDR_SIZE as u64,
            p_paddr: 0x400000 + EHDR_SIZE as u64,
            p_filesz: ph_size,
            p_memsz: ph_size,
            p_align: 8,
        });
        b.add_phdr(load_phdr(PF_R, 0, 0x400000, 0x1000, 0x1000));
        b.add_phdr(load_phdr(PF_R | PF_X, 0x1000, 0x401000, 0x100, 0x100));
        b.add_phdr(load_phdr(PF_R | PF_W, 0x1100, 0x402000, 0x200, 0x1800));

        let img = b.build_raw();
        let loaded = LoadedElf::parse(&img).unwrap();

        assert_eq!(loaded.elf_type, ElfType::Exec);
        assert_eq!(loaded.load_bias, 0);
        assert_eq!(loaded.entry, 0x401010);
        assert_eq!(loaded.phdr_vaddr, Some(0x400040));
        assert_eq!(loaded.at_phdr(), 0x400040);
        assert_eq!(loaded.phent, 56);
        assert_eq!(loaded.phnum, 4);
        assert_eq!(loaded.initial_brk, 0x404000);
        assert_eq!(loaded.segments().len(), 3);
        assert_eq!(loaded.segments()[2].bss_len(), 0x1600);

        let mem = assert_plan_matches_ranges(&loaded, &img);
        assert_eq!(mem[&0x400000].1, Perm::R);
        assert_eq!(mem[&0x401000].1, Perm::RX);
        assert_eq!(&mem[&0x401000].0[..0x100], &text_bytes[..]);
        assert_eq!(mem[&0x402000].1, Perm::RW);
        assert_eq!(&mem[&0x402000].0[..0x200], &data_bytes[..]);
        assert!(mem[&0x402000].0[0x200..].iter().all(|&b| b == 0));
        assert_eq!(mem[&0x403000].1, Perm::RW);
        assert!(mem[&0x403000].0.iter().all(|&b| b == 0));
    }

    #[test]
    fn valid_static_pie_et_dyn_and_at_phdr_fallback() {
        // Static PIE with p_vaddr starting at 0, NO PT_PHDR -> AT_PHDR must be
        // computed via PT_LOAD covering e_phoff..e_phoff + ph_bytes.
        let mut b = ElfBuilder::new(ET_DYN, 0x1020);
        let ph_size = 2 * usize::from(PHDR_MIN_SIZE);
        let pad = 0x1000 - (EHDR_SIZE + ph_size);
        b.push_data(&vec![0u8; pad]);
        b.push_data(&[0xc3; 0x80]); // ret

        b.add_phdr(load_phdr(PF_R, 0, 0x0, 0x400, 0x400));
        b.add_phdr(load_phdr(PF_R | PF_X, 0x1000, 0x1000, 0x80, 0x80));

        let img = b.build_raw();
        let loaded = LoadedElf::parse(&img).unwrap();

        assert_eq!(loaded.elf_type, ElfType::Pie);
        assert_eq!(loaded.load_bias, PIE_BASE);
        assert_eq!(loaded.entry, PIE_BASE + 0x1020);
        assert_eq!(loaded.phdr_vaddr, Some(PIE_BASE + EHDR_SIZE as u64));
        assert_eq!(loaded.initial_brk, PIE_BASE + 0x2000);
        assert_plan_matches_ranges(&loaded, &img);

        // Custom PIE base works too.
        let custom_base = 0x0000_2000_0000_0000;
        let loaded2 = LoadedElf::parse_with_pie_base(&img, custom_base).unwrap();
        assert_eq!(loaded2.load_bias, custom_base);
        assert_eq!(loaded2.entry, custom_base + 0x1020);
        assert_eq!(loaded2.phdr_vaddr, Some(custom_base + EHDR_SIZE as u64));
    }

    #[test]
    fn pt_phdr_outside_loaded_segments_falls_back_to_pt_load() {
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        let ph_size = 3 * u64::from(PHDR_MIN_SIZE);
        let pad = 0x1000 - (EHDR_SIZE + ph_size as usize);
        b.push_data(&vec![0u8; pad]);
        b.push_data(&[0xc3; 0x40]);

        // Bogus PT_PHDR pointing outside any PT_LOAD -> must fall back to PT_LOAD covering e_phoff.
        b.add_phdr(ProgramHeader {
            p_type: PT_PHDR,
            p_flags: PF_R,
            p_offset: EHDR_SIZE as u64,
            p_vaddr: 0x900000,
            p_paddr: 0x900000,
            p_filesz: ph_size,
            p_memsz: ph_size,
            p_align: 8,
        });
        b.add_phdr(load_phdr(PF_R, 0, 0x400000, 0x300, 0x300));
        b.add_phdr(load_phdr(PF_R | PF_X, 0x1000, 0x401000, 0x40, 0x40));

        let img = b.build_raw();
        let loaded = LoadedElf::parse(&img).unwrap();
        assert_eq!(loaded.phdr_vaddr, Some(0x400040));
    }

    #[test]
    fn pt_phdr_into_bss_or_wrong_file_bytes_falls_back_to_actual_phdr_table() {
        let ph_size = 4 * u64::from(PHDR_MIN_SIZE);
        let pad = 0x1000 - (EHDR_SIZE + ph_size as usize);

        // Case 1: PT_PHDR points into the .bss tail of a readable RW- segment.
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        b.push_data(&vec![0u8; pad]);
        b.push_data(&[0xc3; 0x40]); // .text at file offset 0x1000
        b.push_data(&[0x77; 0x200]); // .data at file offset 0x1040

        b.add_phdr(ProgramHeader {
            p_type: PT_PHDR,
            p_flags: PF_R,
            p_offset: EHDR_SIZE as u64,
            p_vaddr: 0x402300, // inside .bss (filesz ends at 0x402200)
            p_paddr: 0x402300,
            p_filesz: ph_size,
            p_memsz: ph_size,
            p_align: 8,
        });
        b.add_phdr(load_phdr(PF_R, 0, 0x400000, 0x400, 0x400));
        b.add_phdr(load_phdr(PF_R | PF_X, 0x1000, 0x401000, 0x40, 0x40));
        b.add_phdr(load_phdr(PF_R | PF_W, 0x1040, 0x402000, 0x200, 0x1000));

        let img = b.build_raw();
        let loaded = LoadedElf::parse(&img).unwrap();
        // Must reject the .bss PT_PHDR and fall back to the actual mapped header table.
        assert_eq!(loaded.phdr_vaddr, Some(0x400040));

        // Case 2: PT_PHDR points to wrong initialized bytes within a readable PT_LOAD
        // (maps file offset 0x1040 instead of e_phoff = 0x40).
        let mut b2 = ElfBuilder::new(ET_EXEC, 0x401000);
        b2.push_data(&vec![0u8; pad]);
        b2.push_data(&[0xc3; 0x40]);
        b2.push_data(&[0x77; 0x200]);

        b2.add_phdr(ProgramHeader {
            p_type: PT_PHDR,
            p_flags: PF_R,
            p_offset: EHDR_SIZE as u64,
            p_vaddr: 0x402000, // initialized .data bytes, NOT e_phoff!
            p_paddr: 0x402000,
            p_filesz: ph_size,
            p_memsz: ph_size,
            p_align: 8,
        });
        b2.add_phdr(load_phdr(PF_R, 0, 0x400000, 0x400, 0x400));
        b2.add_phdr(load_phdr(PF_R | PF_X, 0x1000, 0x401000, 0x40, 0x40));
        b2.add_phdr(load_phdr(PF_R | PF_W, 0x1040, 0x402000, 0x200, 0x200));

        let img2 = b2.build_raw();
        let loaded2 = LoadedElf::parse(&img2).unwrap();
        assert_eq!(loaded2.phdr_vaddr, Some(0x400040));
    }

    #[test]
    fn unreadable_pt_load_covering_phdrs_yields_none_for_at_phdr() {
        // An execute-only (PF_X, no PF_R) PT_LOAD covering offset 0..0x400 cannot
        // supply readable AT_PHDR even with a matching PT_PHDR.
        let mut b = ElfBuilder::new(ET_EXEC, 0x400200);
        let ph_size = 2 * u64::from(PHDR_MIN_SIZE);
        let pad = 0x200 - (EHDR_SIZE + ph_size as usize);
        b.push_data(&vec![0u8; pad]);
        b.push_data(&[0xc3; 0x200]);

        b.add_phdr(ProgramHeader {
            p_type: PT_PHDR,
            p_flags: PF_R,
            p_offset: EHDR_SIZE as u64,
            p_vaddr: 0x400000 + EHDR_SIZE as u64,
            p_paddr: 0x400000 + EHDR_SIZE as u64,
            p_filesz: ph_size,
            p_memsz: ph_size,
            p_align: 8,
        });
        b.add_phdr(load_phdr(PF_X, 0, 0x400000, 0x400, 0x400));

        let img = b.build_raw();
        let loaded = LoadedElf::parse(&img).unwrap();
        assert_eq!(loaded.phdr_vaddr, None);
        assert_eq!(loaded.at_phdr(), 0);
    }

    #[test]
    fn relro_shared_4k_boundary_page_unions_perms_and_preserves_bytes() {
        // Real Alpine/musl RELRO pattern:
        // - .text R-X at 0x401000..0x401100
        // - .rodata/RELRO R-- at 0x402000..0x403300 (spans page 0x402000 and first 0x300 bytes of 0x403000)
        // - .data/.bss RW- at 0x403300..0x404500 (starts at offset 0x300 of page 0x403000, filesz 0x400, rest BSS)
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        let ph_size = 3 * usize::from(PHDR_MIN_SIZE);
        let base_off = EHDR_SIZE + ph_size;

        let text = vec![0x90u8; 0x100];
        let text_off = base_off + b.push_data(&text);

        let rodata = vec![0xAAu8; 0x1300];
        let rodata_off = base_off + b.push_data(&rodata);

        let data = vec![0x55u8; 0x400];
        let data_off = base_off + b.push_data(&data);

        // Deliberately insert out of virtual-address order to verify sorting.
        b.add_phdr(load_phdr(PF_R | PF_W, data_off as u64, 0x403300, 0x400, 0x1200));
        b.add_phdr(load_phdr(PF_R | PF_X, text_off as u64, 0x401000, 0x100, 0x100));
        b.add_phdr(load_phdr(PF_R, rodata_off as u64, 0x402000, 0x1300, 0x1300));

        let img = b.build_raw();
        let loaded = LoadedElf::parse(&img).unwrap();

        assert_eq!(loaded.initial_brk, 0x405000);
        assert_eq!(
            loaded.mapped_ranges(),
            vec![
                MappedRange { start: 0x401000, end: 0x402000, perm: Perm::RX },
                MappedRange { start: 0x402000, end: 0x403000, perm: Perm::R },
                MappedRange { start: 0x403000, end: 0x405000, perm: Perm::RW },
            ]
        );

        let actions = loaded.plan_pages();
        assert!(actions.contains(&PageAction::UpdatePerm { vaddr: 0x403000, perm: Perm::RW }));

        let mem = assert_plan_matches_ranges(&loaded, &img);
        // Page 0x402000: pure R--, all 0xAA.
        assert_eq!(mem[&0x402000].1, Perm::R);
        assert!(mem[&0x402000].0.iter().all(|&b| b == 0xAA));
        // Shared boundary page 0x403000: unioned RW-, preserves both segments' bytes + BSS zeroes!
        assert_eq!(mem[&0x403000].1, Perm::RW);
        assert!(mem[&0x403000].0[0x000..0x300].iter().all(|&b| b == 0xAA));
        assert!(mem[&0x403000].0[0x300..0x700].iter().all(|&b| b == 0x55));
        assert!(mem[&0x403000].0[0x700..0x1000].iter().all(|&b| b == 0x00));
        // Page 0x404000: pure BSS zeroes, RW-.
        assert_eq!(mem[&0x404000].1, Perm::RW);
        assert!(mem[&0x404000].0.iter().all(|&b| b == 0x00));
    }

    #[test]
    fn shared_boundary_page_wx_is_rejected() {
        // Direct R-X + RW- sharing page 0x401000 -> union is RWX -> refused.
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        let base_off = EHDR_SIZE + 2 * usize::from(PHDR_MIN_SIZE);
        let off1 = base_off + b.push_data(&[0x90; 0x200]);
        let off2 = base_off + b.push_data(&[0x55; 0x200]);
        b.add_phdr(load_phdr(PF_R | PF_X, off1 as u64, 0x401000, 0x200, 0x200));
        b.add_phdr(load_phdr(PF_R | PF_W, off2 as u64, 0x401200, 0x200, 0x200));
        let img = b.build_raw();
        assert_eq!(LoadedElf::parse(&img), Err(ElfLoadError::SharedPageWritableAndExecutable { page: 0x401000 }));
    }

    #[test]
    fn three_segments_in_one_page_transitive_wx_is_rejected() {
        // R-X, R--, RW- all in page 0x401000 -> cumulative union is RWX -> refused.
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        let base_off = EHDR_SIZE + 3 * usize::from(PHDR_MIN_SIZE);
        let off1 = base_off + b.push_data(&[0x90; 0x100]);
        let off2 = base_off + b.push_data(&[0xAA; 0x100]);
        let off3 = base_off + b.push_data(&[0x55; 0x100]);
        b.add_phdr(load_phdr(PF_R | PF_X, off1 as u64, 0x401000, 0x100, 0x100));
        b.add_phdr(load_phdr(PF_R, off2 as u64, 0x401100, 0x100, 0x100));
        b.add_phdr(load_phdr(PF_R | PF_W, off3 as u64, 0x401200, 0x100, 0x100));
        let img = b.build_raw();
        assert_eq!(LoadedElf::parse(&img), Err(ElfLoadError::SharedPageWritableAndExecutable { page: 0x401000 }));
    }

    #[test]
    fn rejects_all_malformed_headers_and_segments() {
        // Truncated header
        assert_eq!(LoadedElf::parse(&[0u8; 63]), Err(ElfLoadError::Truncated));

        // Bad magic
        let mut bad_magic = [0u8; 64];
        bad_magic[0..4].copy_from_slice(b"NOPE");
        assert_eq!(LoadedElf::parse(&bad_magic), Err(ElfLoadError::BadMagic));

        // Helper for a minimal valid exec image
        let make_valid = || {
            let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
            let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
            b.push_data(&[0xc3; 0x40]);
            b.add_phdr(load_phdr(PF_R | PF_X, off as u64, 0x401000, 0x40, 0x40));
            b
        };

        // Bad class (ELFCLASS32 = 1)
        let mut b = make_valid();
        b.ei_class = 1;
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::BadClass(1)));

        // Bad endian (ELFDATA2MSB = 2)
        let mut b = make_valid();
        b.ei_data = 2;
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::BadEndian(2)));

        // Bad EI_VERSION
        let mut b = make_valid();
        b.ei_version = 2;
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::BadVersion));

        // Bad e_version
        let mut b = make_valid();
        b.e_version = 0;
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::BadVersion));

        // Unsupported machine (AArch64 = 183)
        let mut b = make_valid();
        b.e_machine = 183;
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::UnsupportedMachine(183)));

        // Unsupported type (ET_REL = 1)
        let mut b = make_valid();
        b.e_type = 1;
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::UnsupportedType(1)));

        // PT_INTERP refused with DynamicInterpreterUnsupported
        let mut b = make_valid();
        b.add_phdr(ProgramHeader {
            p_type: PT_INTERP,
            p_flags: PF_R,
            p_offset: 0,
            p_vaddr: 0,
            p_paddr: 0,
            p_filesz: 0,
            p_memsz: 0,
            p_align: 1,
        });
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::DynamicInterpreterUnsupported));

        // Small e_phentsize (< 56)
        let mut b = make_valid();
        b.e_phentsize = 32;
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::BadProgramHeaderTable));

        // Overflowing e_phoff + e_phnum * e_phentsize
        let mut img = make_valid().build_raw();
        img[0x20..0x28].copy_from_slice(&(u64::MAX - 16).to_le_bytes());
        assert_eq!(LoadedElf::parse(&img), Err(ElfLoadError::BadProgramHeaderTable));

        // Truncated program header table
        let mut img = make_valid().build_raw();
        img.truncate(EHDR_SIZE + 20);
        assert_eq!(LoadedElf::parse(&img), Err(ElfLoadError::Truncated));

        // Zero program headers
        let b = ElfBuilder::new(ET_EXEC, 0x401000);
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::NoLoadSegments));

        // Only zero-memsz PT_LOAD
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        b.add_phdr(load_phdr(PF_R | PF_X, 0, 0x401000, 0, 0));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::NoLoadSegments));

        // p_filesz > p_memsz
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x80]);
        b.add_phdr(load_phdr(PF_R | PF_X, off as u64, 0x401000, 0x80, 0x40));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::FileSizeExceedsMemSize { index: 0 }));

        // Segment file bytes out of bounds
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        b.add_phdr(load_phdr(PF_R | PF_X, 0x1000, 0x401000, 0x40, 0x40));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::SegmentFileOutOfBounds { index: 0 }));

        // Single W+X PT_LOAD segment
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x40]);
        b.add_phdr(load_phdr(PF_R | PF_W | PF_X, off as u64, 0x401000, 0x40, 0x40));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::WritableAndExecutable { index: 0 }));

        // Byte-overlapping segments
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        let off = EHDR_SIZE + 2 * usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x200]);
        b.add_phdr(load_phdr(PF_R | PF_X, off as u64, 0x401000, 0x100, 0x100));
        b.add_phdr(load_phdr(PF_R, off as u64, 0x4010ff, 0x100, 0x100));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::OverlappingSegments { first: 0, second: 1 }));

        // Address below USER_MIN
        let mut b = ElfBuilder::new(ET_EXEC, 0x2000);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x40]);
        b.add_phdr(load_phdr(PF_R | PF_X, off as u64, 0x2000, 0x40, 0x40));
        assert_eq!(
            LoadedElf::parse(&b.build_raw()),
            Err(ElfLoadError::AddressOutOfRange { index: 0, start: 0x2000, end: 0x2040 })
        );

        // Address past USER_MAX (into unmapped canonical guard page)
        let mut b = ElfBuilder::new(ET_EXEC, USER_MAX - 0x100);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x40]);
        b.add_phdr(load_phdr(PF_R | PF_X, off as u64, USER_MAX - 0x100, 0x40, 0x200));
        assert_eq!(
            LoadedElf::parse(&b.build_raw()),
            Err(ElfLoadError::AddressOutOfRange { index: 0, start: USER_MAX - 0x100, end: USER_MAX + 0x100 })
        );

        // Arithmetic overflow on vaddr + memsz
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x40]);
        b.add_phdr(load_phdr(PF_R | PF_X, off as u64, u64::MAX - 0x10, 0x40, 0x40));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::AddressOverflow { index: 0 }));

        // Arithmetic overflow on PIE load_bias + p_vaddr
        let mut b = ElfBuilder::new(ET_DYN, 0x1000);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x40]);
        b.add_phdr(load_phdr(PF_R | PF_X, off as u64, u64::MAX - 0x10, 0x40, 0x40));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::AddressOverflow { index: 0 }));

        // Invalid PIE base (misaligned or out of range)
        let mut b = ElfBuilder::new(ET_DYN, 0x0);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x40]);
        b.add_phdr(load_phdr(PF_R | PF_X, off as u64, 0, 0x40, 0x40));
        let pie_img = b.build_raw();
        assert_eq!(LoadedElf::parse_with_pie_base(&pie_img, 0x1_0001), Err(ElfLoadError::InvalidPieBase(0x1_0001)));
        assert_eq!(LoadedElf::parse_with_pie_base(&pie_img, 0), Err(ElfLoadError::InvalidPieBase(0)));
        assert_eq!(LoadedElf::parse_with_pie_base(&pie_img, USER_MAX), Err(ElfLoadError::InvalidPieBase(USER_MAX)));

        // Entry outside any segment
        let mut b = make_valid();
        b.e_entry = 0x409999;
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::EntryNotExecutable { entry: 0x409999 }));

        // Segment file offset + filesz overflow
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        b.add_phdr(load_phdr(PF_R | PF_X, u64::MAX - 10, 0x401000, 0x40, 0x40));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::SegmentFileOutOfBounds { index: 0 }));

        // PIE entry address overflow
        let mut b = ElfBuilder::new(ET_DYN, u64::MAX - 10);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x40]);
        b.add_phdr(load_phdr(PF_R | PF_X, off as u64, 0, 0x40, 0x40));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::EntryOverflow));

        // Entry inside non-executable (R--) segment
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x40]);
        b.add_phdr(load_phdr(PF_R, off as u64, 0x401000, 0x40, 0x40));
        assert_eq!(LoadedElf::parse(&b.build_raw()), Err(ElfLoadError::EntryNotExecutable { entry: 0x401000 }));
    }

    #[test]
    fn accepts_larger_phentsize_and_shared_page_without_perm_upgrade() {
        // e_phentsize = 64 (> 56) and two segments sharing a page where the
        // first is already RW- and the second is R-- (no UpdatePerm needed).
        let mut b = ElfBuilder::new(ET_EXEC, 0x401000);
        b.e_phentsize = 64;
        let base_off = EHDR_SIZE + 3 * 64;
        let text_off = base_off + b.push_data(&[0xc3; 0x40]);
        let rw_off = base_off + b.push_data(&[0x11; 0x200]);
        let ro_off = base_off + b.push_data(&[0x22; 0x200]);

        b.add_phdr(load_phdr(PF_R | PF_X, text_off as u64, 0x401000, 0x40, 0x40));
        b.add_phdr(load_phdr(PF_R | PF_W, rw_off as u64, 0x402000, 0x200, 0x200));
        b.add_phdr(load_phdr(PF_R, ro_off as u64, 0x402200, 0x200, 0x1000));

        let img = b.build_raw();
        let loaded = LoadedElf::parse(&img).unwrap();
        assert_eq!(loaded.phent, 64);
        assert_eq!(loaded.phnum, 3);
        assert_eq!(loaded.phdr_vaddr, None);
        assert_eq!(loaded.at_phdr(), 0);
        // No UpdatePerm needed since first segment already had RW- which covers R--.
        assert!(!loaded.plan_pages().iter().any(|a| matches!(a, PageAction::UpdatePerm { .. })));
        let mem = assert_plan_matches_ranges(&loaded, &img);
        assert_eq!(mem[&0x402000].1, Perm::RW);
        assert!(mem[&0x402000].0[0x000..0x200].iter().all(|&x| x == 0x11));
        assert!(mem[&0x402000].0[0x200..0x400].iter().all(|&x| x == 0x22));
        assert_eq!(mem[&0x403000].1, Perm::R);
    }

    #[test]
    fn exact_user_min_and_user_max_boundaries_succeed() {
        // Segment starting exactly at USER_MIN and another ending exactly at USER_MAX.
        let mut b = ElfBuilder::new(ET_EXEC, USER_MIN);
        let off = EHDR_SIZE + 2 * usize::from(PHDR_MIN_SIZE);
        b.push_data(&[0xc3; 0x40]);
        b.add_phdr(load_phdr(PF_R | PF_X, off as u64, USER_MIN, 0x40, 0x40));
        b.add_phdr(load_phdr(PF_R | PF_W, off as u64, USER_MAX - 0x800, 0x40, 0x800));

        let img = b.build_raw();
        let loaded = LoadedElf::parse(&img).unwrap();
        assert!(!loaded.has_interp());
        assert_eq!(loaded.interp_path(&img), None);
        assert_eq!(loaded.segments()[0].vaddr, USER_MIN);
        assert_eq!(loaded.segments()[1].end(), USER_MAX);
        assert_eq!(loaded.initial_brk, USER_MAX);
        assert_plan_matches_ranges(&loaded, &img);
    }

    fn make_dynamic_elf(e_type: u16, base_vaddr: u64, interp_bytes: &[u8]) -> Vec<u8> {
        let mut b = ElfBuilder::new(e_type, base_vaddr + 0x1000);
        let ph_size = 3 * usize::from(PHDR_MIN_SIZE);
        let interp_off = EHDR_SIZE + ph_size + b.push_data(interp_bytes);
        let pad_len = 0x1000 - (EHDR_SIZE + ph_size + interp_bytes.len());
        b.push_data(&vec![0u8; pad_len]);
        b.push_data(&[0xc3; 0x80]);

        b.add_phdr(ProgramHeader {
            p_type: PT_INTERP,
            p_flags: PF_R,
            p_offset: interp_off as u64,
            p_vaddr: base_vaddr + interp_off as u64,
            p_paddr: base_vaddr + interp_off as u64,
            p_filesz: interp_bytes.len() as u64,
            p_memsz: interp_bytes.len() as u64,
            p_align: 1,
        });
        b.add_phdr(load_phdr(PF_R, 0, base_vaddr, 0x1000, 0x1000));
        b.add_phdr(load_phdr(PF_R | PF_X, 0x1000, base_vaddr + 0x1000, 0x80, 0x80));
        b.build_raw()
    }

    #[test]
    fn valid_dynamic_et_exec_and_pie_with_pt_interp() {
        let interp = b"/lib/ld-musl-x86_64.so.1\0";

        // Dynamic ET_EXEC
        let exec_img = make_dynamic_elf(ET_EXEC, 0x40_0000, interp);
        assert_eq!(LoadedElf::parse(&exec_img), Err(ElfLoadError::DynamicInterpreterUnsupported));
        let loaded_exec = LoadedElf::parse_with_pie_base(&exec_img, PIE_BASE).unwrap();
        assert_eq!(loaded_exec.elf_type, ElfType::Exec);
        assert_eq!(loaded_exec.load_bias, 0);
        assert!(loaded_exec.has_interp());
        assert_eq!(loaded_exec.interp_path(&exec_img), Some(b"/lib/ld-musl-x86_64.so.1".as_slice()));
        assert_eq!(loaded_exec.phdr_vaddr, Some(0x40_0000 + EHDR_SIZE as u64));

        // Dynamic ET_DYN (PIE)
        let pie_img = make_dynamic_elf(ET_DYN, 0, interp);
        assert_eq!(LoadedElf::parse(&pie_img), Err(ElfLoadError::DynamicInterpreterUnsupported));
        let loaded_pie = LoadedElf::parse_with_pie_base(&pie_img, PIE_BASE).unwrap();
        assert_eq!(loaded_pie.elf_type, ElfType::Pie);
        assert_eq!(loaded_pie.load_bias, PIE_BASE);
        assert!(loaded_pie.has_interp());
        assert_eq!(loaded_pie.interp_path(&pie_img), Some(b"/lib/ld-musl-x86_64.so.1".as_slice()));
        assert_eq!(loaded_pie.phdr_vaddr, Some(PIE_BASE + EHDR_SIZE as u64));
        assert_eq!(loaded_pie.entry, PIE_BASE + 0x1000);
        assert_plan_matches_ranges(&loaded_pie, &pie_img);
    }

    #[test]
    fn pt_interp_rejects_malformed_headers_and_unmapped_phdr() {
        let make_with_interp = |interp_data: &[u8], phdr_override: Option<(u64, u64)>| {
            let ph_size = 3 * usize::from(PHDR_MIN_SIZE);
            let off_est = EHDR_SIZE + ph_size;
            let pad_to = (off_est + interp_data.len()).next_multiple_of(0x1000);
            let mut b = ElfBuilder::new(ET_DYN, pad_to as u64);
            let off = EHDR_SIZE + ph_size + b.push_data(interp_data);
            b.push_data(&vec![0u8; pad_to - (off + interp_data.len())]);
            b.push_data(&[0xc3; 0x80]);

            let (p_offset, p_filesz) = phdr_override.unwrap_or((off as u64, interp_data.len() as u64));
            b.add_phdr(ProgramHeader {
                p_type: PT_INTERP,
                p_flags: PF_R,
                p_offset,
                p_vaddr: p_offset,
                p_paddr: p_offset,
                p_filesz,
                p_memsz: p_filesz,
                p_align: 1,
            });
            b.add_phdr(load_phdr(PF_R, 0, 0, pad_to as u64, pad_to as u64));
            b.add_phdr(load_phdr(PF_R | PF_X, pad_to as u64, pad_to as u64, 0x80, 0x80));
            b
        };

        // Duplicate PT_INTERP
        let mut dup = make_with_interp(b"/lib/ld.so\0", None);
        // Adding a 4th phdr shifts payload by PHDR_MIN_SIZE, so adjust p_offset for the existing phdrs.
        for ph in &mut dup.phdrs {
            if ph.p_type == PT_INTERP || (ph.p_type == PT_LOAD && ph.p_offset > 0) {
                ph.p_offset += u64::from(PHDR_MIN_SIZE);
            }
        }
        let first_interp = dup.phdrs[0];
        dup.add_phdr(first_interp);
        assert_eq!(LoadedElf::parse_with_pie_base(&dup.build_raw(), PIE_BASE), Err(ElfLoadError::DuplicateInterp));

        // Empty (filesz == 0) and NUL-only (filesz == 1)
        let empty = make_with_interp(b"", Some((0, 0)));
        assert_eq!(LoadedElf::parse_with_pie_base(&empty.build_raw(), PIE_BASE), Err(ElfLoadError::InvalidInterp));
        let nul_only = make_with_interp(b"\0", None);
        assert_eq!(LoadedElf::parse_with_pie_base(&nul_only.build_raw(), PIE_BASE), Err(ElfLoadError::InvalidInterp));

        // Exactly PATH_MAX (4096 = 4095 bytes + NUL) succeeds; PATH_MAX + 1 fails.
        let mut max_path = vec![b'a'; PATH_MAX - 1];
        max_path.push(0);
        let max_img = make_with_interp(&max_path, None).build_raw();
        let ok = LoadedElf::parse_with_pie_base(&max_img, PIE_BASE).unwrap();
        assert_eq!(ok.interp_path(&max_img).unwrap().len(), PATH_MAX - 1);

        let mut too_long = vec![b'a'; PATH_MAX];
        too_long.push(0);
        let too_long_img = make_with_interp(&too_long, None).build_raw();
        assert_eq!(LoadedElf::parse_with_pie_base(&too_long_img, PIE_BASE), Err(ElfLoadError::InvalidInterp));

        // Out-of-bounds slice and arithmetic overflow
        let oob = make_with_interp(b"/lib/ld\0", Some((0x10_0000, 16)));
        assert_eq!(LoadedElf::parse_with_pie_base(&oob.build_raw(), PIE_BASE), Err(ElfLoadError::InterpOutOfBounds));
        let ovf = make_with_interp(b"/lib/ld\0", Some((u64::MAX - 4, 16)));
        assert_eq!(LoadedElf::parse_with_pie_base(&ovf.build_raw(), PIE_BASE), Err(ElfLoadError::InterpOutOfBounds));

        // Missing trailing NUL and interior NUL
        let no_nul = make_with_interp(b"/lib/ld.so", None);
        assert_eq!(LoadedElf::parse_with_pie_base(&no_nul.build_raw(), PIE_BASE), Err(ElfLoadError::InvalidInterp));
        let interior_nul = make_with_interp(b"/lib/\0ld.so\0", None);
        assert_eq!(
            LoadedElf::parse_with_pie_base(&interior_nul.build_raw(), PIE_BASE),
            Err(ElfLoadError::InvalidInterp)
        );

        // PT_INTERP with unmapped AT_PHDR (no PT_LOAD covering e_phoff)
        let mut b = ElfBuilder::new(ET_DYN, 0x1000);
        let ph_size = 2 * usize::from(PHDR_MIN_SIZE);
        let interp = b"/lib/ld-musl-x86_64.so.1\0";
        let interp_off = EHDR_SIZE + ph_size + b.push_data(interp);
        let pad = 0x1000 - (interp_off + interp.len());
        b.push_data(&vec![0u8; pad]);
        b.push_data(&[0xc3; 0x80]);
        b.add_phdr(ProgramHeader {
            p_type: PT_INTERP,
            p_flags: PF_R,
            p_offset: interp_off as u64,
            p_vaddr: 0,
            p_paddr: 0,
            p_filesz: interp.len() as u64,
            p_memsz: interp.len() as u64,
            p_align: 1,
        });
        b.add_phdr(load_phdr(PF_R | PF_X, 0x1000, 0x1000, 0x80, 0x80));
        assert_eq!(LoadedElf::parse_with_pie_base(&b.build_raw(), PIE_BASE), Err(ElfLoadError::DynamicMissingPhdr));

        // PT_INTERP with execute-only (non-readable) PT_LOAD covering e_phoff -> DynamicMissingPhdr
        let mut b_xo = ElfBuilder::new(ET_EXEC, 0x40_0200);
        let ph_size = 2 * usize::from(PHDR_MIN_SIZE);
        let interp_off = EHDR_SIZE + ph_size + b_xo.push_data(interp);
        let pad = 0x200 - (interp_off + interp.len());
        b_xo.push_data(&vec![0u8; pad]);
        b_xo.push_data(&[0xc3; 0x200]);
        b_xo.add_phdr(ProgramHeader {
            p_type: PT_INTERP,
            p_flags: PF_R,
            p_offset: interp_off as u64,
            p_vaddr: 0x40_0000 + interp_off as u64,
            p_paddr: 0x40_0000 + interp_off as u64,
            p_filesz: interp.len() as u64,
            p_memsz: interp.len() as u64,
            p_align: 1,
        });
        b_xo.add_phdr(load_phdr(PF_X, 0, 0x40_0000, 0x400, 0x400));
        assert_eq!(LoadedElf::parse_with_pie_base(&b_xo.build_raw(), PIE_BASE), Err(ElfLoadError::DynamicMissingPhdr));
    }

    #[test]
    fn parse_interp_validates_et_dyn_nested_interp_and_min_vaddr_bias() {
        let make_interp = |min_vaddr: u64| {
            let aligned_min = align_down(min_vaddr);
            let mut b = ElfBuilder::new(ET_DYN, aligned_min + 0x1020);
            let ph_size = 2 * usize::from(PHDR_MIN_SIZE);
            let pad = 0x1000 - (EHDR_SIZE + ph_size);
            b.push_data(&vec![0u8; pad]);
            b.push_data(&[0xc3; 0x80]);
            b.add_phdr(load_phdr(PF_R, 0, min_vaddr, 0x400, 0x400));
            b.add_phdr(load_phdr(PF_R | PF_X, 0x1000, aligned_min + 0x1000, 0x80, 0x80));
            b.build_raw()
        };

        // min_vaddr == 0 -> load_bias == INTERP_BASE
        let img0 = make_interp(0);
        let ld0 = LoadedElf::parse_interp(&img0, INTERP_BASE).unwrap();
        assert_eq!(ld0.load_bias, INTERP_BASE);
        assert_eq!(ld0.entry, INTERP_BASE + 0x1020);
        assert_eq!(ld0.segments()[0].page_start(), INTERP_BASE);
        assert!(!ld0.has_interp());

        // min_vaddr == 0x2000 -> load_bias == INTERP_BASE - 0x2000, lowest page maps at INTERP_BASE.
        let img2k = make_interp(0x2000);
        let ld2k = LoadedElf::parse_interp(&img2k, INTERP_BASE).unwrap();
        assert_eq!(ld2k.load_bias, INTERP_BASE - 0x2000);
        assert_eq!(ld2k.segments()[0].vaddr, INTERP_BASE);
        assert_eq!(ld2k.segments()[0].page_start(), INTERP_BASE);
        assert_eq!(ld2k.entry, INTERP_BASE + 0x1020);

        // Unaligned min_vaddr == 0x2100 -> align_down(0x2100) = 0x2000 -> lowest page starts at INTERP_BASE.
        let img_unaligned = make_interp(0x2100);
        let ld_unaligned = LoadedElf::parse_interp(&img_unaligned, INTERP_BASE).unwrap();
        assert_eq!(ld_unaligned.load_bias, INTERP_BASE - 0x2000);
        assert_eq!(ld_unaligned.segments()[0].vaddr, INTERP_BASE + 0x100);
        assert_eq!(ld_unaligned.segments()[0].page_start(), INTERP_BASE);

        // Nested PT_INTERP inside interpreter is rejected with NestedInterp.
        let nested_img = make_dynamic_elf(ET_DYN, 0, b"/lib/other.so\0");
        assert_eq!(LoadedElf::parse_interp(&nested_img, INTERP_BASE), Err(ElfLoadError::NestedInterp));

        // ET_EXEC interpreter rejected with InterpreterNotDynamic.
        let mut exec_b = ElfBuilder::new(ET_EXEC, 0x40_1000);
        let off = EHDR_SIZE + usize::from(PHDR_MIN_SIZE);
        exec_b.push_data(&[0xc3; 0x40]);
        exec_b.add_phdr(load_phdr(PF_R | PF_X, off as u64, 0x40_1000, 0x40, 0x40));
        assert_eq!(
            LoadedElf::parse_interp(&exec_b.build_raw(), INTERP_BASE),
            Err(ElfLoadError::InterpreterNotDynamic(ET_EXEC))
        );

        // Underflow when base < align_down(min_vaddr), plus invalid base alignment/bounds.
        let high_min = make_interp(0x80_0000);
        assert_eq!(LoadedElf::parse_interp(&high_min, 0x40_0000), Err(ElfLoadError::InvalidPieBase(0x40_0000)));
        assert_eq!(LoadedElf::parse_interp(&img0, INTERP_BASE + 1), Err(ElfLoadError::InvalidPieBase(INTERP_BASE + 1)));
        assert_eq!(LoadedElf::parse_interp(&img0, 0), Err(ElfLoadError::InvalidPieBase(0)));
    }
}
