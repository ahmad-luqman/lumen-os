//! Loading a Linux binary for `linux` (and, later, `execve`): the binary and
//! the interpreter its `PT_INTERP` names are read and parsed once, before
//! anything is mapped, then loaded into a fresh [`AddressSpace`]
//! (ARCHITECTURE section 8, "Dynamic linking").

use alloc::borrow::Cow;
use alloc::string::String;
use core::fmt;

use lumen_elf::loader::{ElfLoadError, LoadedElf};
use lumen_vfs::VfsError;
use lumen_vm::{INTERP_BASE, PIE_BASE};

use crate::linux::errno;
use crate::mm::user_vm::{AddressSpace, ElfImage, LoadedProgram, UserVmError};
use crate::vfs;

/// Why a binary could not be loaded; each names the file it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    /// The binary could not be read (`ENOENT`, `EACCES` when it is not a
    /// regular file with an execute bit, ...).
    Read { path: String, err: VfsError },
    /// The interpreter its `PT_INTERP` names could not be read (a path that
    /// is not UTF-8 names no file: `ENOENT`).
    InterpRead { path: String, err: VfsError },
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
            ExecError::InterpBad { path, err } => {
                write!(f, "bad interpreter '{path}': {err}: ELIBBAD ({})", errno::ELIBBAD)
            }
            ExecError::Load(e) => write!(f, "{e} (errno {})", e.to_errno()),
        }
    }
}

/// The interpreter a binary's `PT_INTERP` names, read and planned at
/// `INTERP_BASE`.
struct Interp {
    path: String,
    bytes: alloc::vec::Vec<u8>,
    plan: LoadedElf,
}

/// A binary ready to load: its bytes and plan (parsed once), the path it was
/// read from, and its interpreter's file, read and planned. Every file is
/// read when this is made, so `linux` counts frames after the page cache has
/// taken its share.
pub struct Exec {
    bytes: Cow<'static, [u8]>,
    plan: LoadedElf,
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
        read_interp: impl FnOnce(&str) -> Result<alloc::vec::Vec<u8>, VfsError>,
    ) -> Result<Self, ExecError> {
        let plan =
            LoadedElf::parse_with_pie_base(&bytes, PIE_BASE).map_err(|e| ExecError::Load(UserVmError::Elf(e)))?;
        let interp = match plan.interp_path(&bytes) {
            None => None,
            Some(raw) => {
                let Ok(path) = core::str::from_utf8(raw) else {
                    // The VFS resolves UTF-8 paths only; no other path names a file in it.
                    let path = String::from_utf8_lossy(raw).into_owned();
                    return Err(ExecError::InterpRead { path, err: VfsError::NoEnt });
                };
                let path = String::from(path);
                let interp_bytes = match read_interp(&path) {
                    Ok(b) => b,
                    Err(err) => return Err(ExecError::InterpRead { path, err }),
                };
                match LoadedElf::parse_interp(&interp_bytes, INTERP_BASE) {
                    Ok(interp_plan) => Some(Interp { path, bytes: interp_bytes, plan: interp_plan }),
                    Err(err) => return Err(ExecError::InterpBad { path, err }),
                }
            }
        };
        Ok(Self { bytes, plan, exe_path, interp })
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

    /// Load the binary and its interpreter into a brand-new address space
    /// ([`AddressSpace::from_images`]).
    pub fn load(
        &self,
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
    ) -> Result<(AddressSpace, LoadedProgram), ExecError> {
        let main = ElfImage { plan: &self.plan, bytes: &self.bytes };
        let interp = self.interp.as_ref().map(|i| ElfImage { plan: &i.plan, bytes: &i.bytes });
        AddressSpace::from_images(main, interp, argv, envp, execfn, random).map_err(ExecError::Load)
    }
}

/// A dynamically linked main image naming `interp`: `PT_PHDR`, `PT_INTERP`,
/// an `R--` page at `vbase` holding the headers and an `R-X` page above it
/// starting `MAIN-TXT`, with its entry at `vbase + 0x1010` (an `ET_DYN`
/// gets `PIE_BASE` added).
#[cfg(feature = "ktest")]
fn test_dynamic_main(e_type: u16, vbase: u64, interp: &[u8]) -> alloc::vec::Vec<u8> {
    use lumen_elf::loader::{PF_R, PF_X, PT_INTERP, PT_LOAD, PT_PHDR};
    let mut path = alloc::vec::Vec::from(interp);
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
/// `text_memsz` bytes above it starting `INTERP!!`, its entry at
/// `vbase + 0x1020`, plus `extra` program headers.
#[cfg(feature = "ktest")]
fn test_interp(
    e_type: u16,
    vbase: u64,
    text_memsz: u64,
    extra: &[(u32, u32, u64, u64, u64, u64)],
) -> alloc::vec::Vec<u8> {
    use lumen_elf::loader::{PF_R, PF_X, PT_LOAD};
    let mut phdrs = alloc::vec![
        (PT_LOAD, PF_R, 0, vbase, 0x400, 0x1000),
        (PT_LOAD, PF_R | PF_X, 0x1000, vbase + 0x1000, 0x100, text_memsz),
    ];
    phdrs.extend_from_slice(extra);
    crate::user::build_elf_with_phdrs(e_type, vbase + 0x1020, &phdrs, 0x2000, &[(0x1000, b"INTERP!!")])
}

/// The auxiliary vector on a freshly built initial stack at `rsp` (past
/// `argc`, `argv` and `envp`), read through the current address space.
#[cfg(feature = "ktest")]
fn read_test_auxv(rsp: u64) -> Result<alloc::vec::Vec<(u64, u64)>, &'static str> {
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
    let mut auxv = alloc::vec::Vec::new();
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
fn test_exec(main: alloc::vec::Vec<u8>, interp: Result<alloc::vec::Vec<u8>, VfsError>) -> Result<Exec, ExecError> {
    Exec::new(Cow::Owned(main), None, |_| interp)
}

crate::ktest!(
    fn dynamic_elf_maps_interp_and_sets_auxv() {
        use alloc::sync::Arc;
        use alloc::vec::Vec;

        use crate::mm::frame;
        use crate::user::copy_from_user;
        use crate::{kassert, kassert_eq};
        use lumen_elf::loader::ET_DYN;
        use lumen_elf::stack::{AT_BASE, AT_ENTRY, AT_PHDR, AT_PHNUM};

        const MAIN: &str = "lumen-ktest-main";
        const INTERP: &str = "ld-lumen-ktest.so";
        // The interpreter is linked at 0x10000, so its load bias (AT_BASE)
        // is INTERP_BASE - 0x10000 while its pages start at INTERP_BASE.
        let interp_img = test_interp(ET_DYN, 0x10000, 0x100, &[]);
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
            let main = ElfImage { plan: &exec.plan, bytes: &exec.bytes };
            let interp = exec.interp.as_ref().map(|i| ElfImage { plan: &i.plan, bytes: &i.bytes });
            let stat = ElfImage { plan: &elf.plan, bytes: &elf.bytes };
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
        use alloc::vec::Vec;

        use crate::kassert_eq;
        use crate::mm::frame;
        use crate::mm::user_vm::{LayoutClash, USER_STACK_BOTTOM};
        use lumen_elf::loader::{ET_DYN, ET_EXEC, PF_R, PT_INTERP};
        use lumen_vm::{PAGE_SIZE, USER_MAX, USER_STACK_TOP};

        const LD: &[u8] = b"/lib/ld-musl-x86_64.so.1";
        let interp = || -> Result<Vec<u8>, VfsError> { Ok(test_interp(ET_DYN, 0, 0x100, &[])) };
        // Load an ET_EXEC main image spanning `vbase..vbase + 0x2000`.
        let load_main_at = |vbase: u64| -> Result<(), ExecError> {
            let exec = test_exec(test_dynamic_main(ET_EXEC, vbase, LD), interp())?;
            let (space, _) = exec.load(&[b"/t"], &[], b"/t", [0; 16])?;
            space.teardown();
            Ok(())
        };
        let load_interp = |img: Vec<u8>| -> Result<(), ExecError> {
            let exec = test_exec(test_dynamic_main(ET_DYN, 0, LD), Ok(img))?;
            exec.load(&[b"/t"], &[], b"/t", [0; 16]).map(|_| ())
        };
        let layout = |c: LayoutClash| -> Result<(), ExecError> { Err(ExecError::Load(UserVmError::Layout(c))) };

        let _warm: Vec<u8> = alloc::vec![0u8; 64 * 1024];
        drop(_warm);
        let free_before = frame::free_frames();

        // An ET_EXEC main image placed over the interpreter's pages.
        kassert_eq!(load_main_at(INTERP_BASE), layout(LayoutClash::ImagesOverlap));
        // A main image may end exactly at the stack's guard page or start
        // exactly at the stack's top, but not reach into the guard page.
        let guard_bottom = USER_STACK_BOTTOM - PAGE_SIZE;
        kassert_eq!(load_main_at(guard_bottom - 0x2000), Ok(()));
        kassert_eq!(load_main_at(USER_STACK_TOP), Ok(()));
        kassert_eq!(load_main_at(guard_bottom - 0x1000), layout(LayoutClash::MainOnStack));
        kassert_eq!(load_main_at(USER_STACK_TOP - 0x1000), layout(LayoutClash::MainOnStack));
        // An interpreter reaching the stack's guard page (refused before
        // anything is mapped).
        let long = test_interp(ET_DYN, 0, guard_bottom - INTERP_BASE - 0x1000 + PAGE_SIZE, &[]);
        kassert_eq!(load_interp(long), layout(LayoutClash::InterpNotBelowStack));
        kassert_eq!(frame::free_frames(), free_before);

        // An interpreter past the top of user space, with its own
        // interpreter, or not an ET_DYN: ELIBBAD, naming it.
        let path = String::from("/lib/ld-musl-x86_64.so.1");
        let past = load_interp(test_interp(ET_DYN, 0, USER_MAX - INTERP_BASE, &[]));
        kassert_eq!(past.as_ref().map_err(ExecError::to_errno), Err(errno::ELIBBAD));
        let shown = past.map_err(|e| alloc::format!("{e}"));
        kassert_eq!(
            shown.map_err(|s| s.starts_with("bad interpreter '/lib/ld-musl-x86_64.so.1': PT_LOAD[1]")),
            Err(true)
        );
        let nested = test_interp(ET_DYN, 0, 0x100, &[(PT_INTERP, PF_R, 0x400, 0x400, 2, 2)]);
        kassert_eq!(
            load_interp(nested),
            Err(ExecError::InterpBad { path: path.clone(), err: ElfLoadError::NestedInterp })
        );
        kassert_eq!(
            load_interp(test_interp(ET_EXEC, 0x40_0000, 0x100, &[])),
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
        // A path that is not UTF-8 names no file in the VFS.
        let odd = test_exec(test_dynamic_main(ET_DYN, 0, b"/lib/\xffld.so"), Ok(alloc::vec::Vec::new())).map(|_| ());
        kassert_eq!(odd, Err(ExecError::InterpRead { path: String::from("/lib/\u{fffd}ld.so"), err: VfsError::NoEnt }));
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
