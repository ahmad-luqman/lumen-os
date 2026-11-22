//! Loading a Linux binary for `linux` (and, later, `execve`): the binary and
//! the interpreter its `PT_INTERP` names are read and parsed once, before
//! anything is mapped, then loaded into a fresh [`AddressSpace`]
//! (ARCHITECTURE section 8, "Dynamic linking").

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use lumen_elf::loader::{ElfLoadError, LoadedElf};
use lumen_vfs::VfsError;
use lumen_vm::{INTERP_BASE, PIE_BASE};

use crate::linux::errno;
use crate::mm::user_vm::{AddressSpace, ElfImage, LoadedProgram, UserVmError};
use crate::vfs;

/// Why a binary could not be loaded. The read and interpreter errors name
/// the file they are about; `Load` is about the binary, which the caller
/// names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    /// The binary could not be read (`ENOENT`, `EACCES` when it is not a
    /// regular file with an execute bit, `ENOMEM`, ...).
    Read { path: String, err: VfsError },
    /// The interpreter its `PT_INTERP` names could not be read.
    InterpRead { path: String, err: VfsError },
    /// The interpreter's path is not UTF-8, so it names no file in the VFS
    /// (`ENOENT`); `path` shows it lossily.
    InterpPathNotUtf8 { path: String },
    /// The interpreter is not a loadable `ET_DYN` without its own
    /// `PT_INTERP` (`ELIBBAD`).
    InterpBad { path: String, err: ElfLoadError },
    /// The binary could not be loaded.
    Load(UserVmError),
}

impl ExecError {
    /// The Linux `errno` an `execve(2)` failing this way returns (the
    /// syscall arrives with #254; the ktests use it now).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub const fn to_errno(&self) -> u32 {
        match self {
            ExecError::Read { err, .. } | ExecError::InterpRead { err, .. } => err.to_errno(),
            ExecError::InterpPathNotUtf8 { .. } => errno::ENOENT,
            ExecError::InterpBad { .. } => errno::ELIBBAD,
            ExecError::Load(e) => e.to_errno(),
        }
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Read { path, err } => write!(f, "cannot read '{path}': {err}"),
            ExecError::InterpRead { path, err } => write!(f, "cannot read its interpreter '{path}': {err}"),
            ExecError::InterpPathNotUtf8 { path } => {
                write!(f, "cannot read its interpreter '{path}' (not UTF-8): {}", VfsError::NoEnt)
            }
            ExecError::InterpBad { path, err } => {
                write!(f, "bad interpreter '{path}': {err}: ELIBBAD ({})", errno::ELIBBAD)
            }
            ExecError::Load(e) => write!(f, "{e} (errno {})", e.to_errno()),
        }
    }
}

/// One ELF file: its bytes and its plan, parsed once.
struct Image {
    bytes: Cow<'static, [u8]>,
    plan: LoadedElf,
}

impl Image {
    fn elf(&self) -> ElfImage<'_> {
        ElfImage { plan: &self.plan, bytes: &self.bytes }
    }
}

/// The interpreter a binary's `PT_INTERP` names, read and planned at
/// `INTERP_BASE`.
struct Interp {
    path: String,
    image: Image,
}

impl Interp {
    /// The interpreter at `raw` (the `PT_INTERP` path), read with `read`.
    fn read(raw: &[u8], read: impl FnOnce(&str) -> Result<Vec<u8>, VfsError>) -> Result<Self, ExecError> {
        let Ok(path) = core::str::from_utf8(raw) else {
            return Err(ExecError::InterpPathNotUtf8 { path: String::from_utf8_lossy(raw).into_owned() });
        };
        let path = String::from(path);
        let bytes = match read(&path) {
            Ok(bytes) => bytes,
            Err(err) => return Err(ExecError::InterpRead { path, err }),
        };
        match LoadedElf::parse_interp(&bytes, INTERP_BASE) {
            Ok(plan) => Ok(Self { path, image: Image { bytes: Cow::Owned(bytes), plan } }),
            Err(err) => Err(ExecError::InterpBad { path, err }),
        }
    }
}

/// A binary ready to load: its file (parsed once), the path it was read
/// from, and its interpreter's file, read and planned. Every file is read
/// when this is made, so `linux` counts frames after the page cache has
/// taken its share.
pub struct Exec {
    main: Image,
    exe_path: Option<String>,
    interp: Option<Interp>,
}

impl Exec {
    /// The executable at absolute `path` in the VFS (symlinks followed), its
    /// interpreter read through the VFS too; `exe_path` is `path` as given.
    pub fn from_path(path: &str) -> Result<Self, ExecError> {
        let bytes = vfs::read_executable(path).map_err(|err| ExecError::Read { path: String::from(path), err })?;
        Self::new(Cow::Owned(bytes), Some(String::from(path)), vfs::read_executable)
    }

    /// A binary already in memory (a boot module), its interpreter, if it
    /// names one, read through the VFS; no `exe_path`.
    pub fn from_image(bytes: Cow<'static, [u8]>) -> Result<Self, ExecError> {
        Self::new(bytes, None, vfs::read_executable)
    }

    /// Parse `bytes` (`ET_DYN` at `PIE_BASE`) and read and plan the
    /// interpreter it names with `read_interp`.
    fn new(
        bytes: Cow<'static, [u8]>,
        exe_path: Option<String>,
        read_interp: impl FnOnce(&str) -> Result<Vec<u8>, VfsError>,
    ) -> Result<Self, ExecError> {
        let plan =
            LoadedElf::parse_with_pie_base(&bytes, PIE_BASE).map_err(|e| ExecError::Load(UserVmError::Elf(e)))?;
        let interp = plan.interp_path(&bytes).map(|raw| Interp::read(raw, read_interp)).transpose()?;
        Ok(Self { main: Image { bytes, plan }, exe_path, interp })
    }

    /// The path the binary was read from, as given, for `/proc/<pid>/exe`.
    pub fn exe_path(&self) -> Option<&str> {
        self.exe_path.as_deref()
    }

    /// The interpreter's path, for a dynamically linked binary.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub fn interp_path(&self) -> Option<&str> {
        self.interp.as_ref().map(|i| i.path.as_str())
    }

    /// The main image and its interpreter's, as [`AddressSpace::from_images`]
    /// takes them.
    fn images(&self) -> (ElfImage<'_>, Option<ElfImage<'_>>) {
        (self.main.elf(), self.interp.as_ref().map(|i| i.image.elf()))
    }

    /// Load the binary and its interpreter into a brand-new address space
    /// ([`AddressSpace::from_images`]).
    pub fn load(
        &self,
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
    ) -> Result<(AddressSpace, LoadedProgram), ExecError> {
        let (main, interp) = self.images();
        AddressSpace::from_images(main, interp, argv, envp, execfn, random).map_err(ExecError::Load)
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
/// ktests that need no file.
#[cfg(feature = "ktest")]
fn test_exec(main: Vec<u8>, interp: Result<Vec<u8>, VfsError>) -> Result<Exec, ExecError> {
    Exec::new(Cow::Owned(main), None, |_| interp)
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
        let main_file = tmp.create(MAIN, 0o755).map_err(|_| "create main")?;
        let interp_file = tmp.create(INTERP, 0o755).map_err(|_| "create interpreter")?;
        let written = (main_file.write_at(0, &main_img), interp_file.write_at(0, &interp_img));
        let exec = Exec::from_path("/tmp/lumen-ktest-main");
        let mode = interp_file.set_mode(0o644);
        let not_executable = Exec::from_path("/tmp/lumen-ktest-main").map(|_| ());
        drop((main_file, interp_file));
        let unlinked = (tmp.unlink(MAIN), tmp.unlink(INTERP));
        kassert_eq!(written, (Ok(main_img.len()), Ok(interp_img.len())));
        kassert_eq!(unlinked, (Ok(()), Ok(())));
        kassert_eq!(mode, Ok(()));
        kassert_eq!(
            not_executable,
            Err(ExecError::InterpRead { path: String::from("/tmp/ld-lumen-ktest.so"), err: VfsError::Acces })
        );
        let exec = exec.map_err(|_| "Exec::from_path failed")?;
        kassert_eq!(exec.exe_path(), Some("/tmp/lumen-ktest-main"));
        kassert_eq!(exec.interp_path(), Some("/tmp/ld-lumen-ktest.so"));

        let _warm: Vec<u8> = alloc::vec![0u8; 64 * 1024];
        drop(_warm);
        let free_before = frame::free_frames();
        {
            let (space, prog) = exec.load(&[b"/t"], &[b"K=V"], b"/t", [7; 16]).map_err(|_| "dynamic load failed")?;
            kassert_eq!(prog.entry, INTERP_BASE + 0x1020);
            kassert_eq!(prog.initial_brk, PIE_BASE + 0x2000);
            // The interpreter's pages keep their own rights: W^X.
            let maps = space.format_maps();
            let rx_page = alloc::format!("{:08x}-{:08x} r-xp ", INTERP_BASE + 0x1000, INTERP_BASE + 0x2000);
            let ro_page = alloc::format!("{:08x}-{:08x} r--p ", INTERP_BASE, INTERP_BASE + 0x1000);
            kassert!(maps.contains(&rx_page) && maps.contains(&ro_page));
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
            let (space, prog) = elf.load(&[b"/s"], &[], b"/s", [7; 16]).map_err(|_| "static load failed")?;
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
            exec.load(&[b"/t"], &[], b"/t", [0; 16]).map(|(space, _)| space)
        };
        let load_main_at = |vbase: u64| main_at(vbase).map(|space| space.teardown());
        let load_interp = |img: Vec<u8>| -> Result<(), ExecError> {
            let exec = test_exec(test_dynamic_main(ET_DYN, 0, LD), Ok(img))?;
            exec.load(&[b"/t"], &[], b"/t", [0; 16]).map(|(space, _)| space.teardown())
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
        // A path that is not UTF-8 names no file in the VFS, and says so.
        let odd = test_exec(test_dynamic_main(ET_DYN, 0, b"/lib/\xffld.so"), Ok(Vec::new())).map(|_| ());
        kassert_eq!(odd, Err(ExecError::InterpPathNotUtf8 { path: String::from("/lib/\u{fffd}ld.so") }));
        kassert_eq!(odd.as_ref().map_err(ExecError::to_errno), Err(errno::ENOENT));
        kassert_eq!(
            odd.map_err(|e| alloc::format!("{e}")),
            Err(String::from("cannot read its interpreter '/lib/\u{fffd}ld.so' (not UTF-8): ENOENT (2)"))
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

        // A 64 KiB executable, read while the heap refuses 32 KiB or more:
        // ENOMEM, not a heap panic, and readable again once it gives.
        const NAME: &str = "lumen-ktest-big";
        let tmp = vfs::resolve_path_from_root("/tmp").map_err(|_| "no /tmp")?;
        let file = tmp.create(NAME, 0o755).map_err(|_| "create")?;
        let written = file.write_at(0, &[0x5a; 64 * 1024]);
        drop(file);
        // And one past the 16 MiB cap is EFBIG from its size, before any
        // buffer is asked for.
        const HUGE: &str = "lumen-ktest-huge";
        let huge = tmp.create(HUGE, 0o755).map_err(|_| "create huge")?;
        let grown = huge.truncate((16 << 20) + 1);
        drop(huge);
        heap::refuse_from(32 * 1024);
        let refused = Exec::from_path("/tmp/lumen-ktest-big").map(|_| ());
        let too_big = vfs::read_executable("/tmp/lumen-ktest-huge").map(|b| b.len());
        heap::refuse_from(usize::MAX);
        let read = vfs::read_executable("/tmp/lumen-ktest-big").map(|b| b.len());
        let unlinked = (tmp.unlink(NAME), tmp.unlink(HUGE));
        kassert_eq!(written, Ok(64 * 1024));
        kassert_eq!(grown, Ok(()));
        kassert_eq!(too_big, Err(VfsError::FBig));
        kassert_eq!(refused, Err(ExecError::Read { path: String::from("/tmp/lumen-ktest-big"), err: VfsError::NoMem }));
        kassert_eq!(read, Ok(64 * 1024));
        kassert_eq!(unlinked, (Ok(()), Ok(())));
        Ok(())
    }
);
