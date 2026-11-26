//! Loading a Linux binary for `linux` and `execve`: bounded headers are
//! validated before segment bytes stream into a fresh [`AddressSpace`].
//! Sources stay open throughout planning and loading
//! (ARCHITECTURE section 8, "Dynamic linking").

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::fmt::Write;

use lumen_elf::loader::{EHDR_SIZE, ElfHeader, ElfLoadError, LoadedElf, validate_interp};
use lumen_vfs::VfsError;
use lumen_vm::{INTERP_BASE, PIE_BASE};

use crate::linux::errno;
use crate::mm::user_vm::{AddressSpace, ElfImage, ImageFile, ImageSource, LoadedProgram, UserVmError};
use crate::vfs;

/// Why a binary could not be loaded. The read and interpreter errors name
/// the file they are about; `Load` can concern either image. Byte paths
/// that are not UTF-8 are shown escaped.
#[derive(Clone, PartialEq, Eq)]
pub enum ExecError {
    /// The binary could not be read (`ENOENT`, `EACCES` when it is not a
    /// regular file with an execute bit, `ENOMEM`, ...).
    Read { path: String, err: VfsError },
    /// The interpreter its `PT_INTERP` names could not be read.
    InterpRead { path: String, err: VfsError },
    /// The interpreter is not a loadable `ET_DYN` without its own
    /// `PT_INTERP` (`ELIBBAD`).
    InterpBad { path: String, err: ElfLoadError },
    /// The binary or its interpreter could not be loaded.
    Load(UserVmError),
}

impl ExecError {
    /// The Linux `errno` an `execve(2)` failing this way returns (the
    /// syscall and the shell share this loader).
    pub const fn to_errno(&self) -> u32 {
        match self {
            ExecError::Read { err, .. } | ExecError::InterpRead { err, .. } => err.to_errno(),
            ExecError::InterpBad { .. } => errno::ELIBBAD,
            ExecError::Load(e) => e.to_errno(),
        }
    }
}

impl fmt::Debug for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Read { path, err } => f.debug_struct("Read").field("path", path).field("err", err).finish(),
            ExecError::InterpRead { path, err } => {
                f.debug_struct("InterpRead").field("path", path).field("err", err).finish()
            }
            ExecError::InterpBad { path, err } => {
                f.debug_struct("InterpBad").field("path", path).field("err", err).finish()
            }
            ExecError::Load(e) => f.debug_tuple("Load").field(e).finish(),
        }
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Read { path, err } => write!(f, "cannot read '{path}': {err}"),
            ExecError::InterpRead { path, err } => write!(f, "cannot read its interpreter '{path}': {err}"),
            ExecError::InterpBad { path, err } => {
                write!(f, "bad interpreter '{path}': {err}: ELIBBAD ({})", errno::ELIBBAD)
            }
            ExecError::Load(e) => write!(f, "{e} (errno {})", e.to_errno()),
        }
    }
}

/// An open executable source and its maps identity.
pub struct ExecFile {
    pub source: ImageSource<'static>,
    pub image: Option<ImageFile>,
}

fn read_file(path: &[u8]) -> Result<ExecFile, VfsError> {
    open_path(None, path)
}

/// Display UTF-8 paths unchanged and escape other byte paths without losing bytes.
pub fn display_path(path: &[u8]) -> Result<String, VfsError> {
    let mut text = String::new();
    text.try_reserve_exact(path.len().checked_mul(4).ok_or(VfsError::NoMem)?).map_err(|_| VfsError::NoMem)?;
    if let Ok(path) = core::str::from_utf8(path) {
        text.push_str(path);
    } else {
        write!(text, "{}", lumen_vfs::Escaped(path)).expect("reserved escaped path");
    }
    Ok(text)
}

/// Resolve bytes from cwd, storing the reached file's identity for procfs and maps.
pub fn open_path(cwd: Option<&Arc<dyn lumen_vfs::VfsInode>>, path: &[u8]) -> Result<ExecFile, VfsError> {
    let named = lumen_vfs::resolve_named_path(
        vfs::tables(),
        cwd,
        path,
        lumen_vfs::ResolveFlags { follow_final: true, empty_path: false, require_dir: false },
    )?;
    let identity = match named.identity {
        Ok(path) => Some(Arc::from(path)),
        Err(VfsError::NoMem) => return Err(VfsError::NoMem),
        Err(error) => {
            log::debug!(target: "exec", "{}: no executable identity: {error}", lumen_vfs::Escaped(path));
            None
        }
    };
    open_inode(named.resolved.inode, identity)
}

/// Hold an executable inode open through loading. Non-regular files or files
/// with no execute bit return EACCES; this does not deny concurrent writes.
pub fn open_inode(
    inode: alloc::sync::Arc<dyn lumen_vfs::VfsInode>,
    path: Option<Arc<[u8]>>,
) -> Result<ExecFile, VfsError> {
    let guard = lumen_vfs::OpenGuard::try_new(inode)?;
    let inode = guard.inode();
    if inode.inode_type() != lumen_vfs::InodeType::File || inode.stat()?.mode & 0o111 == 0 {
        return Err(VfsError::Acces);
    }
    let image = path.map(|path| ImageFile::of(inode.as_ref(), path));
    Ok(ExecFile { source: ImageSource::File(guard), image })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Main,
    Interpreter,
}

struct Image {
    source: ImageSource<'static>,
    plan: LoadedElf,
    file: Option<ImageFile>,
}

impl Image {
    fn elf(&self) -> ElfImage<'_> {
        ElfImage { plan: &self.plan, source: &self.source, file: self.file.as_ref() }
    }

    fn path(&self) -> Option<&[u8]> {
        self.file.as_ref().map(|f| &*f.path)
    }

    fn parse(source: ImageSource<'static>, file: Option<ImageFile>, role: Role) -> Result<Self, ExecError> {
        let path = file.as_ref().map_or(b"".as_slice(), |file| &*file.path);
        let read_error = |err| {
            if role == Role::Interpreter {
                ExecError::InterpRead { path: String::from_utf8_lossy(path).into_owned(), err }
            } else {
                ExecError::Read { path: String::from_utf8_lossy(path).into_owned(), err }
            }
        };
        let bad = |err| {
            if role == Role::Interpreter && err != ElfLoadError::NoMemory {
                ExecError::InterpBad { path: String::from_utf8_lossy(path).into_owned(), err }
            } else {
                ExecError::Load(UserVmError::from(err))
            }
        };
        let size = source.len().map_err(read_error)?;
        if size < EHDR_SIZE as u64 {
            return Err(bad(ElfLoadError::Truncated));
        }
        let mut head = [0; EHDR_SIZE];
        source.read_exact(0, &mut head).map_err(read_error)?;
        let header = ElfHeader::parse(&head).map_err(bad)?;
        let range = header.phdr_range();
        if range.end > size {
            return Err(bad(ElfLoadError::Truncated));
        }
        let mut phdrs = Vec::new();
        phdrs.try_reserve_exact((range.end - range.start) as usize).map_err(|_| read_error(VfsError::NoMem))?;
        phdrs.resize((range.end - range.start) as usize, 0);
        source.read_exact(range.start, &mut phdrs).map_err(read_error)?;
        let plan = if role == Role::Interpreter {
            LoadedElf::parse_interp_headers(&header, &phdrs, size, INTERP_BASE)
        } else {
            LoadedElf::parse_headers_with_pie_base(&header, &phdrs, size, PIE_BASE)
        }
        .map_err(bad)?;
        Ok(Self { source, plan, file })
    }
}

/// A main image and its optional interpreter, with sources held open and validated headers.
pub struct Exec {
    main: Image,
    interp: Option<Image>,
}

impl Exec {
    /// Open the executable at absolute `path`, retaining its inode through loading.
    pub fn from_path(path: &str) -> Result<Self, ExecError> {
        let file = read_file(path.as_bytes()).map_err(|err| ExecError::Read { path: String::from(path), err })?;
        Self::from_source(file.source, file.image, read_file)
    }

    /// Plan a boot module or an in-memory test executable.
    pub fn from_image(bytes: Cow<'static, [u8]>) -> Result<Self, ExecError> {
        Self::from_source(ImageSource::Bytes(bytes), None, read_file)
    }

    /// Plan the main image and open its PT_INTERP using the caller's path resolver.
    pub fn from_source(
        source: ImageSource<'static>,
        file: Option<ImageFile>,
        read_interp: impl FnOnce(&[u8]) -> Result<ExecFile, VfsError>,
    ) -> Result<Self, ExecError> {
        let main = Image::parse(source, file, Role::Main)?;
        let interp = if let Some(range) = main.plan.interp_range() {
            let mut bytes = [0u8; lumen_elf::loader::PATH_MAX];
            let bytes = &mut bytes[..(range.end - range.start) as usize];
            main.source.read_exact(range.start, bytes).map_err(|err| ExecError::Read {
                path: String::from_utf8_lossy(main.path().unwrap_or(b"")).into_owned(),
                err,
            })?;
            let raw = validate_interp(bytes).map_err(|e| ExecError::Load(UserVmError::from(e)))?;
            let file = read_interp(raw)
                .map_err(|err| ExecError::InterpRead { path: String::from_utf8_lossy(raw).into_owned(), err })?;
            Some(Image::parse(file.source, file.image, Role::Interpreter)?)
        } else {
            None
        };
        Ok(Self { main, interp })
    }

    /// The loaded main image's path, for /proc/pid/exe.
    pub fn exe_path(&self) -> Option<Arc<[u8]>> {
        self.main.file.as_ref().map(|file| Arc::clone(&file.path))
    }

    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub fn interp_path(&self) -> Option<&[u8]> {
        self.interp.as_ref().and_then(Image::path)
    }

    fn images(&self) -> (ElfImage<'_>, Option<ElfImage<'_>>) {
        (self.main.elf(), self.interp.as_ref().map(Image::elf))
    }

    /// Load both images with a fresh AT_RANDOM and the default stack limit.
    pub fn load(
        &self,
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
    ) -> Result<(AddressSpace, LoadedProgram), ExecError> {
        self.load_with_stack(argv, envp, execfn, lumen_vm::StackSize::DEFAULT)
    }

    /// Load both images with a fresh AT_RANDOM and this exec's stack size.
    pub fn load_with_stack(
        &self,
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        stack: lumen_vm::StackSize,
    ) -> Result<(AddressSpace, LoadedProgram), ExecError> {
        self.load_inner(argv, envp, execfn, at_random_bytes(), stack)
    }

    fn load_inner(
        &self,
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
        stack: lumen_vm::StackSize,
    ) -> Result<(AddressSpace, LoadedProgram), ExecError> {
        let (main, interp) = self.images();
        AddressSpace::from_images_with_stack(main, interp, argv, envp, execfn, random, stack).map_err(ExecError::Load)
    }

    /// Deterministic AT_RANDOM for stack/auxv assertions only.
    #[cfg(feature = "ktest")]
    fn load_seeded(
        &self,
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
    ) -> Result<(AddressSpace, LoadedProgram), ExecError> {
        self.load_inner(argv, envp, execfn, random, lumen_vm::StackSize::DEFAULT)
    }
}

/// AT_RANDOM is always available, with one warning if boot entropy was unavailable.
fn at_random_bytes() -> [u8; 16] {
    use core::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    let mut out = [0; 16];
    if crate::rng::fill(&mut out).is_err() {
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::warn!(target: "exec", "AT_RANDOM: no seeded RNG; using TSC/time fallback");
        }
        let a = crate::time::now_ns() ^ crate::arch::rdtsc().rotate_left(13);
        let b = crate::arch::rdtsc().wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ a.rotate_right(29);
        out[..8].copy_from_slice(&a.to_le_bytes());
        out[8..].copy_from_slice(&b.to_le_bytes());
    }
    out
}

/// A temporary executable removed through the namespace API even on test failure.
#[cfg(feature = "ktest")]
pub(crate) struct ExecFixture {
    pub inode: alloc::sync::Arc<dyn lumen_vfs::VfsInode>,
    path: &'static str,
}
#[cfg(feature = "ktest")]
impl ExecFixture {
    pub fn symlink(path: &'static str, target: &[u8]) -> Result<Self, &'static str> {
        let (parent, name) = path.rsplit_once('/').ok_or("fixture path")?;
        let dir = vfs::resolve_path_from_root(parent).map_err(|_| "fixture parent")?;
        let inode = dir.symlink(name.as_bytes(), target).map_err(|_| "symlink fixture")?;
        Ok(Self { inode, path })
    }
    pub fn new(path: &'static str, bytes: &[u8]) -> Result<Self, &'static str> {
        let (parent, name) = path.rsplit_once('/').ok_or("fixture path")?;
        let dir = vfs::resolve_path_from_root(parent).map_err(|_| "fixture parent")?;
        let inode = dir.create(name.as_bytes(), 0o755).map_err(|_| "create fixture")?;
        let fixture = Self { inode, path };
        fixture.inode.write_at(0, bytes).map_err(|_| "write fixture")?;
        Ok(fixture)
    }
}
#[cfg(feature = "ktest")]
impl Drop for ExecFixture {
    fn drop(&mut self) {
        let _ = lumen_vfs::unlink_at(vfs::tables(), None, self.path.as_bytes(), false);
    }
}

/// A dynamically linked main image naming `interp`: `PT_PHDR`, `PT_INTERP`,
/// an `R--` page at `vbase` holding the headers and an `R-X` page above it
/// starting `MAIN-TXT`, with its entry at `vbase + 0x1010` (an `ET_DYN`
/// gets `PIE_BASE` added).
#[cfg(feature = "ktest")]
fn test_dynamic_main(e_type: u16, vbase: u64, interp: &[u8]) -> Vec<u8> {
    use lumen_elf::loader::{PF_R, PF_X, PT_INTERP, PT_LOAD, PT_PHDR};
    let mut path = Vec::from(interp);
    path.push(0);
    let len = path.len() as u64;
    crate::user::build_elf_with_phdrs(
        e_type,
        vbase + 0x1010,
        &[
            (PT_PHDR, PF_R, 0x40, vbase + 0x40, 4 * 56, 4 * 56),
            (PT_INTERP, PF_R, 0x200, vbase + 0x200, len, len),
            (PT_LOAD, PF_R, 0, vbase, 0x1000, 0x1000),
            (PT_LOAD, PF_R | PF_X, 0x1000, vbase + 0x1000, 0x100, 0x100),
        ],
        0x2000,
        &[(0x200, &path), (0x1000, b"MAIN-TXT")],
    )
}

/// An interpreter: an `R--` page at `vbase` and an `R-X` segment of
/// `text_memsz` bytes at `vbase + text_off` starting `INTERP!!` (sparse when
/// `text_off` is past the first page), its entry 0x20 into it, plus `extra`
/// program headers.
#[cfg(feature = "ktest")]
fn test_interp(
    e_type: u16,
    vbase: u64,
    text_off: u64,
    text_memsz: u64,
    extra: &[(u32, u32, u64, u64, u64, u64)],
) -> Vec<u8> {
    use lumen_elf::loader::{PF_R, PF_X, PT_LOAD};
    let mut phdrs = alloc::vec![
        (PT_LOAD, PF_R, 0, vbase, 0x400, 0x1000),
        (PT_LOAD, PF_R | PF_X, 0x1000, vbase + text_off, 0x100, text_memsz),
    ];
    phdrs.extend_from_slice(extra);
    crate::user::build_elf_with_phdrs(e_type, vbase + text_off + 0x20, &phdrs, 0x2000, &[(0x1000, b"INTERP!!")])
}

/// The auxiliary vector on a freshly built initial stack at `rsp` (past
/// `argc`, `argv` and `envp`), read through the current address space.
#[cfg(feature = "ktest")]
fn read_test_auxv(rsp: u64) -> Result<Vec<(u64, u64)>, &'static str> {
    use crate::user::copy_from_user;
    let word = |va: u64| -> Result<u64, &'static str> {
        let mut b = [0u8; 8];
        copy_from_user(&mut b, va).map_err(|_| "initial stack unreadable")?;
        Ok(u64::from_le_bytes(b))
    };
    let argc = word(rsp)?;
    let mut va = rsp + 8 * (argc + 2); // argc, argv[..], NULL
    while word(va)? != 0 {
        va += 8; // envp[..]
    }
    va += 8;
    let mut auxv = Vec::new();
    loop {
        let (key, val) = (word(va)?, word(va + 8)?);
        if key == lumen_elf::stack::AT_NULL {
            return Ok(auxv);
        }
        auxv.push((key, val));
        va += 16;
        if auxv.len() > 64 {
            return Err("auxv not terminated");
        }
    }
}

/// An `Exec` of `main` whose interpreter read gives `interp`, for the
/// ktests that need no file (the interpreter's maps lines name its path
/// with device 0:0 and inode 0).
#[cfg(feature = "ktest")]
fn test_exec(main: Vec<u8>, interp: Result<Vec<u8>, VfsError>) -> Result<Exec, ExecError> {
    let file =
        |path: &[u8]| ImageFile { dev: lumen_vfs::DevId::makedev(0, 0), ino: 0, path: alloc::sync::Arc::from(path) };
    Exec::from_source(ImageSource::Bytes(Cow::Owned(main)), None, |path| {
        interp.map(|bytes| ExecFile { source: ImageSource::Bytes(Cow::Owned(bytes)), image: Some(file(path)) })
    })
}

crate::ktest!(
    fn dynamic_elf_maps_interp_and_sets_auxv() {
        use alloc::sync::Arc;

        use crate::mm::frame;
        use crate::user::copy_from_user;
        use crate::{kassert, kassert_eq};
        use lumen_elf::loader::ET_DYN;
        use lumen_elf::stack::{AT_BASE, AT_ENTRY, AT_PHDR, AT_PHNUM};

        const MAIN: &str = "lumen-ktest-main";
        const INTERP: &str = "ld-lumen-ktest.so";
        // The interpreter is linked at 0x10000, so its load bias (AT_BASE)
        // is INTERP_BASE - 0x10000 while its pages start at INTERP_BASE.
        let interp_img = test_interp(ET_DYN, 0x10000, 0x1000, 0x100, &[]);
        let main_img = test_dynamic_main(ET_DYN, 0, b"/tmp/ld-lumen-ktest.so");

        // Both files in tmpfs, read the way `linux /path` reads them, then
        // the interpreter without its execute bits (EACCES, as execve(2));
        // removed before checking anything or counting frames (tmpfs pages
        // hold frames).
        let tmp = vfs::resolve_path_from_root("/tmp").map_err(|_| "no /tmp")?;
        let main_file = tmp.create(MAIN.as_bytes(), 0o755).map_err(|_| "create main")?;
        let interp_file = tmp.create(INTERP.as_bytes(), 0o755).map_err(|_| "create interpreter")?;
        let written = (main_file.write_at(0, &main_img), interp_file.write_at(0, &interp_img));
        let exec = Exec::from_path("/tmp/lumen-ktest-main");
        let mode = interp_file.set_mode(0o644);
        let not_executable = Exec::from_path("/tmp/lumen-ktest-main").map(|_| ());
        let inos = (main_file.ino(), interp_file.ino());
        drop((main_file, interp_file));
        let unlinked = (tmp.unlink(MAIN.as_bytes()), tmp.unlink(INTERP.as_bytes()));
        kassert_eq!(written, (Ok(main_img.len()), Ok(interp_img.len())));
        kassert_eq!(unlinked, (Ok(()), Ok(())));
        kassert_eq!(mode, Ok(()));
        kassert_eq!(
            not_executable,
            Err(ExecError::InterpRead { path: String::from("/tmp/ld-lumen-ktest.so"), err: VfsError::Acces })
        );
        let exec = exec.map_err(|_| "Exec::from_path failed")?;
        kassert_eq!(exec.exe_path().as_deref(), Some(b"/tmp/lumen-ktest-main".as_slice()));
        kassert_eq!(exec.interp_path(), Some(b"/tmp/ld-lumen-ktest.so".as_slice()));

        let _warm: Vec<u8> = alloc::vec![0u8; 64 * 1024];
        drop(_warm);
        let free_before = frame::free_frames();
        {
            let (space, prog) =
                exec.load_seeded(&[b"/t"], &[b"K=V"], b"/t", [7; 16]).map_err(|_| "dynamic load failed")?;
            kassert_eq!(prog.entry, INTERP_BASE + 0x1020);
            kassert_eq!(prog.initial_brk, PIE_BASE + 0x2000);
            // The interpreter's pages keep their own rights (W^X), and each
            // image's lines name its file (tmpfs, 0:2) at its pages' file
            // offsets: the PIE main image at PIE_BASE, the interpreter at
            // INTERP_BASE.
            let maps = space.format_maps_text();
            let line = |start: u64, perms: &str, off: u64, ino: u64, path: &str| {
                let fields = alloc::format!("{start:08x}-{:08x} {perms} {off:08x} 00:02 {ino}", start + 0x1000);
                vfs::ktest_maps_line(&fields, path)
            };
            let (main_ino, interp_ino) = inos;
            let expected = [
                line(PIE_BASE, "r--p", 0, main_ino, "/tmp/lumen-ktest-main"),
                line(PIE_BASE + 0x1000, "r-xp", 0x1000, main_ino, "/tmp/lumen-ktest-main"),
                line(INTERP_BASE, "r--p", 0, interp_ino, "/tmp/ld-lumen-ktest.so"),
                line(INTERP_BASE + 0x1000, "r-xp", 0x1000, interp_ino, "/tmp/ld-lumen-ktest.so"),
            ];
            if let Some(missing) = expected.iter().find(|l| !maps.contains(l.as_str())) {
                crate::ktest::diag(format_args!("missing {missing:?} in:\n{maps}"));
                return Err("an image's maps line is wrong");
            }
            let space = Arc::new(space);
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            let auxv = read_test_auxv(prog.rsp)?;
            let aux = |key: u64| auxv.iter().find(|&&(k, _)| k == key).map(|&(_, v)| v);
            kassert_eq!(aux(AT_BASE), Some(INTERP_BASE - 0x10000));
            kassert_eq!(aux(AT_ENTRY), Some(PIE_BASE + 0x1010));
            kassert_eq!(aux(AT_PHDR), Some(PIE_BASE + 0x40));
            kassert_eq!(aux(AT_PHNUM), Some(4));
            // Both images are mapped where their headers say.
            let mut text = [0u8; 8];
            kassert_eq!(copy_from_user(&mut text, PIE_BASE + 0x1000), Ok(()));
            kassert_eq!(&text, b"MAIN-TXT");
            kassert_eq!(copy_from_user(&mut text, INTERP_BASE + 0x1000), Ok(()));
            kassert_eq!(&text, b"INTERP!!");
            drop(_active);
            space.teardown();
        }
        kassert_eq!(frame::free_frames(), free_before);

        // A static image the same way: its own entry, no AT_BASE, no interpreter.
        {
            let elf = Exec::from_image(Cow::Owned(crate::mm::user_vm::build_relro_test_elf()))
                .map_err(|_| "static Exec failed")?;
            kassert_eq!(elf.interp_path(), None);
            kassert_eq!(elf.exe_path(), None);
            let (space, prog) = elf.load_seeded(&[b"/s"], &[], b"/s", [7; 16]).map_err(|_| "static load failed")?;
            kassert_eq!(prog.entry, 0x40_1000);
            kassert_eq!(prog.initial_brk, 0x40_4000);
            let space = Arc::new(space);
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            let auxv = read_test_auxv(prog.rsp)?;
            kassert!(auxv.iter().all(|&(k, _)| k != AT_BASE));
            kassert_eq!(auxv.iter().find(|&&(k, _)| k == AT_ENTRY).map(|&(_, v)| v), Some(0x40_1000));
            drop(_active);
            space.teardown();

            // An interpreter must be given exactly when the image names one.
            let (main, interp) = exec.images();
            let (stat, _) = elf.images();
            let mismatch = AddressSpace::from_images(stat, interp, &[], &[], b"/s", [7; 16]);
            kassert!(matches!(mismatch, Err(UserVmError::InvalidArg)));
            let mismatch = AddressSpace::from_images(main, None, &[], &[], b"/t", [7; 16]);
            kassert!(matches!(mismatch, Err(UserVmError::InvalidArg)));
        }
        kassert_eq!(frame::free_frames(), free_before);
        Ok(())
    }
);

crate::ktest!(
    fn dynamic_elf_layout_rejected() {
        use crate::mm::frame;
        use crate::mm::user_vm::LayoutClash;
        use crate::{kassert, kassert_eq};
        use lumen_elf::loader::{ET_DYN, ET_EXEC, PF_R, PT_INTERP};
        use lumen_vm::{PAGE_SIZE, USER_MAX, USER_STACK_BOTTOM, USER_STACK_TOP};

        const LD: &[u8] = b"/lib/ld-musl-x86_64.so.1";
        let interp = || -> Result<Vec<u8>, VfsError> { Ok(test_interp(ET_DYN, 0, 0x1000, 0x100, &[])) };
        // Load an ET_EXEC main image spanning `vbase..vbase + 0x2000`.
        let main_at = |vbase: u64| -> Result<AddressSpace, ExecError> {
            let exec = test_exec(test_dynamic_main(ET_EXEC, vbase, LD), interp())?;
            exec.load_seeded(&[b"/t"], &[], b"/t", [0; 16]).map(|(space, _)| space)
        };
        let load_main_at = |vbase: u64| main_at(vbase).map(|space| space.teardown());
        let load_interp = |img: Vec<u8>| -> Result<(), ExecError> {
            let exec = test_exec(test_dynamic_main(ET_DYN, 0, LD), Ok(img))?;
            exec.load_seeded(&[b"/t"], &[], b"/t", [0; 16]).map(|(space, _)| space.teardown())
        };
        let layout = |c: LayoutClash| -> Result<(), ExecError> { Err(ExecError::Load(UserVmError::Layout(c))) };

        let _warm: Vec<u8> = alloc::vec![0u8; 64 * 1024];
        drop(_warm);
        let free_before = frame::free_frames();

        // An ET_EXEC main image placed over the interpreter's pages, and
        // what `linux` prints for it.
        let over = load_main_at(INTERP_BASE);
        kassert_eq!(over, layout(LayoutClash::ImagesOverlap));
        kassert_eq!(
            over.map_err(|e| alloc::format!("{e}")),
            Err(String::from("layout error: the binary and its interpreter overlap (errno 17)"))
        );
        // A main image may end exactly at the stack's guard page or start
        // exactly at the stack's top, but not reach into the guard page.
        let guard_bottom = USER_STACK_BOTTOM - PAGE_SIZE;
        kassert_eq!(load_main_at(guard_bottom - 0x2000), Ok(()));
        kassert_eq!(load_main_at(USER_STACK_TOP), Ok(()));
        kassert_eq!(load_main_at(guard_bottom - 0x1000), layout(LayoutClash::MainOnStack));
        kassert_eq!(load_main_at(USER_STACK_TOP - 0x1000), layout(LayoutClash::MainOnStack));
        // Its `brk` stops at the guard page: none to grow by when the image
        // ends there, one page when it ends a page below.
        let space = main_at(guard_bottom - 0x2000).map_err(|_| "load at the guard")?;
        let at_guard = (space.brk(0), space.brk(guard_bottom + PAGE_SIZE), space.brk(guard_bottom + 1));
        space.teardown();
        drop(space); // its root frame, before the frame count below
        kassert_eq!(at_guard, (guard_bottom, guard_bottom, guard_bottom));
        let space = main_at(guard_bottom - 0x3000).map_err(|_| "load below the guard")?;
        let below = (space.brk(0), space.brk(guard_bottom), space.brk(guard_bottom + 1));
        space.teardown();
        drop(space);
        kassert_eq!(below, (guard_bottom - PAGE_SIZE, guard_bottom, guard_bottom));
        // An interpreter may end exactly at the guard page (sparse: its text
        // page is the last one below it), but may not reach into it, nor
        // have a page beyond the stack (refused before anything is mapped).
        let to_guard = guard_bottom - PAGE_SIZE - INTERP_BASE;
        kassert_eq!(load_interp(test_interp(ET_DYN, 0, to_guard, 0x100, &[])), Ok(()));
        let long = test_interp(ET_DYN, 0, 0x1000, guard_bottom - INTERP_BASE - 0x1000 + PAGE_SIZE, &[]);
        kassert_eq!(load_interp(long), layout(LayoutClash::InterpNotBelowStack));
        let beyond = test_interp(ET_DYN, 0, USER_STACK_TOP - INTERP_BASE, 0x100, &[]);
        kassert_eq!(load_interp(beyond), layout(LayoutClash::InterpNotBelowStack));
        kassert_eq!(frame::free_frames(), free_before);

        // An interpreter past the top of user space, with its own
        // interpreter, or not an ET_DYN: ELIBBAD, naming it.
        let path = String::from("/lib/ld-musl-x86_64.so.1");
        let past = load_interp(test_interp(ET_DYN, 0, 0x1000, USER_MAX - INTERP_BASE, &[]));
        kassert_eq!(past.as_ref().map_err(ExecError::to_errno), Err(errno::ELIBBAD));
        let shown = past.map_err(|e| alloc::format!("{e}"));
        kassert!(shown.is_err_and(|s| s.starts_with("bad interpreter '/lib/ld-musl-x86_64.so.1': PT_LOAD[1]")));
        let nested = test_interp(ET_DYN, 0, 0x1000, 0x100, &[(PT_INTERP, PF_R, 0x400, 0x400, 2, 2)]);
        kassert_eq!(
            load_interp(nested),
            Err(ExecError::InterpBad { path: path.clone(), err: ElfLoadError::NestedInterp })
        );
        kassert_eq!(
            load_interp(test_interp(ET_EXEC, 0x40_0000, 0x1000, 0x100, &[])),
            Err(ExecError::InterpBad { path, err: ElfLoadError::InterpreterNotDynamic(ET_EXEC) })
        );

        kassert_eq!(frame::free_frames(), free_before);
        Ok(())
    }
);

crate::ktest!(
    fn missing_interpreter_is_enoent() {
        use crate::{kassert, kassert_eq};
        use lumen_elf::loader::ET_DYN;

        // Through the real VFS: a missing interpreter, and a directory.
        let missing = Exec::from_image(Cow::Owned(test_dynamic_main(ET_DYN, 0, b"/no/such/ld-lumen.so"))).map(|_| ());
        kassert_eq!(missing.as_ref().map_err(ExecError::to_errno), Err(errno::ENOENT));
        let shown = missing.map_err(|e| alloc::format!("{e}"));
        kassert_eq!(shown, Err(String::from("cannot read its interpreter '/no/such/ld-lumen.so': ENOENT (2)")));
        let dir = Exec::from_image(Cow::Owned(test_dynamic_main(ET_DYN, 0, b"/"))).map(|_| ());
        kassert_eq!(dir, Err(ExecError::InterpRead { path: String::from("/"), err: VfsError::Acces }));
        // The resolver receives byte paths, including a non-UTF-8 PT_INTERP.
        let odd = test_exec(test_dynamic_main(ET_DYN, 0, b"/lib/\xffld.so"), Err(VfsError::NoEnt)).map(|_| ());
        kassert_eq!(odd.as_ref().map_err(ExecError::to_errno), Err(errno::ENOENT));
        let odd =
            test_exec(test_dynamic_main(ET_DYN, 0, b"/lib/\xffld.so"), Ok(test_interp(ET_DYN, 0, 0x1000, 0x1000, &[])))
                .map_err(|_| "non-UTF-8 interpreter")?;
        kassert_eq!(odd.interp_path(), Some(b"/lib/\xffld.so".as_slice()));
        // Keep the interpreter spelling even when escaping would need a refused allocation.
        let image = test_dynamic_main(ET_DYN, 0, b"/no/such/ld-lumen.so");
        let refused = Exec::from_source(ImageSource::Bytes(Cow::Owned(image)), None, |raw| {
            crate::mm::heap::refuse_from(raw.len() * 2);
            Err(VfsError::NoMem)
        })
        .map(|_| ());
        crate::mm::heap::refuse_from(usize::MAX);
        kassert_eq!(
            refused,
            Err(ExecError::InterpRead { path: String::from("/no/such/ld-lumen.so"), err: VfsError::NoMem })
        );
        // The binary itself missing, by path.
        let gone = Exec::from_path("/no/such/binary").map(|_| ());
        kassert_eq!(gone, Err(ExecError::Read { path: String::from("/no/such/binary"), err: VfsError::NoEnt }));
        // Garbage is ENOEXEC.
        let garbage = Exec::from_image(Cow::Borrowed(b"not an elf")).map(|_| ());
        kassert!(matches!(garbage, Err(ExecError::Load(UserVmError::Elf(_)))));
        kassert_eq!(garbage.map_err(|e| e.to_errno()), Err(errno::ENOEXEC));
        Ok(())
    }
);

crate::ktest!(
    fn a_binary_the_heap_cannot_hold_is_enomem() {
        use crate::kassert_eq;
        use crate::mm::heap;

        use lumen_elf::loader::{ET_EXEC, PF_R, PF_X, PT_LOAD};
        let tmp = vfs::resolve_path_from_root("/tmp").map_err(|_| "tmp")?;
        let file = tmp.create(b"lumen-ktest-big", 0o755).map_err(|_| "create")?;
        let mut image = crate::user::build_elf_with_phdrs(
            ET_EXEC,
            0x401000,
            &[(PT_LOAD, PF_R | PF_X, 0, 0x401000, 64, 4096)],
            64 + 65536,
            &[],
        );
        image[54..56].copy_from_slice(&64u16.to_le_bytes());
        image[56..58].copy_from_slice(&1024u16.to_le_bytes());
        file.write_at(0, &image).map_err(|_| "write")?;
        heap::refuse_from(32 * 1024);
        let refused = Exec::from_path("/tmp/lumen-ktest-big").map(|_| ());
        heap::refuse_from(usize::MAX);
        let read = Exec::from_path("/tmp/lumen-ktest-big").map(|_| ());
        lumen_vfs::unlink_at(vfs::tables(), None, b"/tmp/lumen-ktest-big", false).map_err(|_| "unlink")?;
        kassert_eq!(refused.as_ref().map_err(ExecError::to_errno), Err(errno::ENOMEM));
        crate::kassert!(matches!(refused, Err(ExecError::Read { err: VfsError::NoMem, .. })));
        kassert_eq!(read, Ok(()));
        Ok(())
    }
);

crate::ktest!(
    fn streaming_exec_keeps_sources_and_uses_sized_stack() {
        use crate::{kassert, kassert_eq};
        use lumen_elf::loader::{ET_EXEC, PF_R, PF_X, PT_LOAD};
        use lumen_vm::{PAGE_SIZE, USER_STACK_TOP};
        let tmp = vfs::resolve_path_from_root("/tmp").map_err(|_| "tmp")?;
        let file = tmp.create(b"streaming-exec", 0o755).map_err(|_| "create")?;
        const OFFSET: u64 = 20 << 20;
        let header = crate::user::build_elf_with_phdrs(
            ET_EXEC,
            0x401000,
            &[(PT_LOAD, PF_R | PF_X, OFFSET, 0x401000, 8, 0x2000)],
            256,
            &[],
        );
        file.write_at(0, &header).map_err(|_| "header")?;
        file.write_at(OFFSET, b"streamed").map_err(|_| "payload")?;
        crate::mm::heap::refuse_from(32 * 1024);
        let planned = Exec::from_path("/tmp/streaming-exec");
        crate::mm::heap::refuse_from(usize::MAX);
        let exec = planned.map_err(|_| "whole file allocation or header parse")?;
        lumen_vfs::unlink_at(vfs::tables(), None, b"/tmp/streaming-exec", false).map_err(|_| "unlink")?;
        kassert_eq!(file.open_count(), 1);
        for (size, size_rounded) in [(0, 131072), (131073, 135168), (67108864, 67108864), (u64::MAX, 268435456)] {
            let (space, _) = exec
                .load_inner(&[b"stream"], &[], b"stream", [0; 16], lumen_vm::StackSize::from_rlimit(size))
                .map_err(|_| "stream load")?;
            let bottom = USER_STACK_TOP - size_rounded;
            kassert!(space.contains_vma(bottom));
            kassert!(!space.contains_vma(bottom - PAGE_SIZE));
            let maps = space.format_maps_text();
            kassert!(maps.contains(&alloc::format!("{bottom:08x}-")) && maps.contains("[stack]"));
            let space = alloc::sync::Arc::new(space);
            {
                let _active = crate::sched::UserSpaceGuard::enter(space.clone());
                let mut bytes = [0; 8];
                kassert_eq!(crate::user::copy_from_user(&mut bytes, 0x401000), Ok(()));
                kassert_eq!(&bytes, b"streamed");
                kassert_eq!(crate::user::copy_from_user(&mut bytes, 0x402000), Ok(()));
                kassert_eq!(bytes, [0; 8]);
            }
            let child = space.fork_copy().map_err(|_| "fork sized stack")?;
            kassert_eq!(child.format_maps(), space.format_maps());
            child.teardown();
            space.teardown();
        }
        file.truncate(OFFSET + 7).map_err(|_| "shrink")?;
        let free = crate::mm::frame::free_frames();
        let shortened = exec.load_seeded(&[b"stream"], &[], b"stream", [0; 16]);
        kassert!(matches!(shortened, Err(ExecError::Load(UserVmError::ImageRead(VfsError::Io)))));
        kassert_eq!(crate::mm::frame::free_frames(), free);
        drop(exec);
        kassert_eq!(file.open_count(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn exec_load_releases_heap_and_frames() {
        use crate::kassert_eq;
        let exec = Exec::from_image(Cow::Owned(crate::mm::user_vm::build_relro_test_elf())).map_err(|_| "plan")?;
        let (warm, _) = exec.load(&[b"warm"], &[], b"warm").map_err(|_| "warm load")?;
        warm.teardown();
        drop(warm);
        let used = crate::mm::heap::stats().used;
        let frames = crate::mm::frame::free_frames();
        let (space, _) = exec.load(&[b"warm"], &[], b"warm").map_err(|_| "load")?;
        space.teardown();
        drop(space);
        kassert_eq!(crate::mm::heap::stats().used, used);
        kassert_eq!(crate::mm::frame::free_frames(), frames);
        Ok(())
    }
);
