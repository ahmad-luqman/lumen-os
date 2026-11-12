//! Kernel symbol table, for symbolized crash reports and backtraces.
//!
//! Limine gives us the kernel's own ELF file (`ExecutableFileRequest`). We
//! keep a reference to its `.symtab` and look addresses up with
//! [`lumen_elf::SymbolTable::lookup`]. Names are demangled on output.
//!
//! The file lives in bootloader-reclaimable memory; the frame allocator
//! reserves it by name ("kernel file"). Whoever reclaims that memory must keep
//! the file or copy the symbol table first.

use core::fmt;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use lumen_elf::{Elf, ElfError, SymbolTable};

static FILE_PTR: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static FILE_LEN: AtomicUsize = AtomicUsize::new(0);

/// Register the kernel ELF image. Returns the number of symbols found.
pub fn init(file: &'static [u8]) -> Result<usize, ElfError> {
    let table = Elf::parse(file)?.symbol_table()?;
    FILE_LEN.store(file.len(), Ordering::Relaxed);
    FILE_PTR.store(file.as_ptr().cast_mut(), Ordering::Release);
    Ok(table.len())
}

fn table() -> Option<SymbolTable<'static>> {
    let ptr = FILE_PTR.load(Ordering::Acquire);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `init` stored a pointer/length pair from a `&'static [u8]`.
    let file = unsafe { core::slice::from_raw_parts(ptr, FILE_LEN.load(Ordering::Relaxed)) };
    Elf::parse(file).ok()?.symbol_table().ok()
}

/// A resolved location: demangled function name plus offset.
#[derive(Clone, Copy)]
pub struct Location {
    pub name: &'static str,
    pub offset: u64,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `{:#}` drops the trailing `::h0123abcd` hash.
        write!(f, "{:#}+{:#x}", rustc_demangle::demangle(self.name), self.offset)
    }
}

/// Look up the function containing `addr`.
pub fn resolve(addr: u64) -> Option<Location> {
    let (sym, offset) = table()?.lookup(addr)?;
    Some(Location { name: sym.name, offset })
}

/// Formats as `<symbol+0x12>` or `<unknown>`.
pub struct Symbolized(pub u64);

impl fmt::Display for Symbolized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match resolve(self.0) {
            Some(loc) => write!(f, "{loc}"),
            None => f.write_str("<unknown>"),
        }
    }
}

#[cfg(feature = "ktest")]
#[inline(never)]
fn ktest_symbol_marker() -> u64 {
    core::hint::black_box(0x5eed)
}

crate::ktest!(
    fn symbols_resolve_kernel_functions() {
        let addr = ktest_symbol_marker as fn() -> u64 as usize as u64;
        let Some(loc) = resolve(addr + 1) else { return Err("marker address did not resolve") };
        crate::kassert_eq!(loc.offset, 1);
        crate::kassert!(loc.name.contains("ktest_symbol_marker"));
        Ok(())
    }
);
